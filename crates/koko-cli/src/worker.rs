//! One bounded, serial owner for the embedded database and connection.

use crate::parameter::ParameterStore;
use crate::signal::{self, InterruptCursor};
use koko::config::MemoryUsage;
use koko::execution::Outcome;
use koko::result::Column;
use koko::tooling::{CatalogSnapshot, SessionSnapshot};
use koko::{Database, InterruptHandle, QueryResult};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const REQUEST_CAPACITY: usize = 1;

enum Request {
    Execute {
        cypher: String,
        parameters: ParameterStore,
        reply: SyncSender<Result<Outcome, WorkerError>>,
    },
    Session {
        reply: SyncSender<koko::Result<SessionSnapshot>>,
    },
    Catalog {
        reply: SyncSender<koko::Result<CatalogSnapshot>>,
    },
    CatalogGraph {
        graph: String,
        reply: SyncSender<koko::Result<CatalogSnapshot>>,
    },
    Memory {
        reply: SyncSender<MemoryUsage>,
    },
    Shutdown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionEvent {
    Progress(Duration),
    CancellationRequested(Duration),
}

#[derive(Debug)]
pub struct ExecutionReport {
    pub outcome: Outcome,
    pub elapsed: Duration,
    pub cancellation_requested: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum WorkerError {
    #[error("CLI session worker stopped unexpectedly")]
    Stopped,
    #[error("could not install the interrupt handler: {0}")]
    Signal(String),
    #[error("terminal output failed while a query was running: {0}")]
    Observer(String),
    #[error("internal CLI session worker failure")]
    Panicked,
    #[error(transparent)]
    Engine(#[from] koko::Error),
}

pub struct SessionWorker {
    requests: SyncSender<Request>,
    interrupt: InterruptHandle,
    join: Option<JoinHandle<()>>,
}

impl SessionWorker {
    pub fn start() -> Result<Self, WorkerError> {
        signal::install().map_err(WorkerError::Signal)?;
        let (requests, receiver) = mpsc::sync_channel(REQUEST_CAPACITY);
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let join = thread::Builder::new()
            .name("koko-session".to_string())
            .spawn(move || worker_main(receiver, ready_tx))
            .map_err(|_| WorkerError::Stopped)?;
        let interrupt = ready_rx.recv().map_err(|_| WorkerError::Stopped)?;
        Ok(Self {
            requests,
            interrupt,
            join: Some(join),
        })
    }

    pub fn interrupt_handle(&self) -> InterruptHandle {
        self.interrupt.clone()
    }

    pub fn execute(
        &self,
        cypher: String,
        parameters: ParameterStore,
    ) -> Result<Outcome, WorkerError> {
        Ok(self
            .execute_observed(cypher, parameters, |_| Ok(()))?
            .outcome)
    }

    pub fn execute_observed(
        &self,
        cypher: String,
        parameters: ParameterStore,
        mut observer: impl FnMut(ExecutionEvent) -> Result<(), String>,
    ) -> Result<ExecutionReport, WorkerError> {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        let mut interrupts = InterruptCursor::current();
        self.requests
            .send(Request::Execute {
                cypher,
                parameters,
                reply: reply_tx,
            })
            .map_err(|_| WorkerError::Stopped)?;
        let started = Instant::now();
        let mut next_progress = Duration::from_millis(500);
        let mut cancellation_requested = false;
        let mut observer_error = None;
        loop {
            match reply_rx.recv_timeout(Duration::from_millis(50)) {
                Ok(result) => {
                    let outcome = result?;
                    if let Some(error) = observer_error {
                        return Err(WorkerError::Observer(error));
                    }
                    return Ok(ExecutionReport {
                        outcome,
                        elapsed: started.elapsed(),
                        cancellation_requested,
                    });
                }
                Err(RecvTimeoutError::Disconnected) => return Err(WorkerError::Stopped),
                Err(RecvTimeoutError::Timeout) => {}
            }

            let elapsed = started.elapsed();
            if interrupts.take() && !cancellation_requested {
                cancellation_requested = true;
                self.interrupt.interrupt();
                if observer_error.is_none() {
                    observer_error = observer(ExecutionEvent::CancellationRequested(elapsed)).err();
                }
            }
            if elapsed >= next_progress {
                if observer_error.is_none() {
                    observer_error = observer(ExecutionEvent::Progress(elapsed)).err();
                    if observer_error.is_some() && !cancellation_requested {
                        cancellation_requested = true;
                        self.interrupt.interrupt();
                    }
                }
                next_progress += Duration::from_millis(200);
            }
        }
    }

    pub fn session_snapshot(&self) -> Result<SessionSnapshot, WorkerError> {
        Ok(self.call(|reply| Request::Session { reply })??)
    }

    pub fn catalog_snapshot(&self) -> Result<CatalogSnapshot, WorkerError> {
        Ok(self.call(|reply| Request::Catalog { reply })??)
    }

    pub fn catalog_snapshot_for_graph(&self, graph: &str) -> Result<CatalogSnapshot, WorkerError> {
        Ok(self.call(|reply| Request::CatalogGraph {
            graph: graph.to_string(),
            reply,
        })??)
    }

    pub fn memory_usage(&self) -> Result<MemoryUsage, WorkerError> {
        self.call(|reply| Request::Memory { reply })
    }

    fn call<T>(&self, request: impl FnOnce(SyncSender<T>) -> Request) -> Result<T, WorkerError> {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        self.requests
            .send(request(reply_tx))
            .map_err(|_| WorkerError::Stopped)?;
        reply_rx.recv().map_err(|_| WorkerError::Stopped)
    }
}

impl Drop for SessionWorker {
    fn drop(&mut self) {
        let _ = self.requests.send(Request::Shutdown);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn worker_main(receiver: Receiver<Request>, ready: SyncSender<InterruptHandle>) {
    let database = Database::new();
    let connection = database.connect();
    if ready.send(connection.interrupt_handle()).is_err() {
        return;
    }
    while let Ok(request) = receiver.recv() {
        match request {
            Request::Execute {
                cypher,
                parameters,
                reply,
            } => {
                let result = guarded_execution(|| {
                    let query_parameters = parameters.query_parameters();
                    connection.execute_detailed_with(&cypher, query_parameters)
                });
                let panicked = result.is_err();
                let _ = reply.send(result);
                if panicked {
                    break;
                }
            }
            Request::Session { reply } => {
                let _ = reply.send(connection.session_snapshot());
            }
            Request::Catalog { reply } => {
                let _ = reply.send(connection.catalog_snapshot());
            }
            Request::CatalogGraph { graph, reply } => {
                let _ = reply.send(connection.catalog_snapshot_for_graph(&graph));
            }
            Request::Memory { reply } => {
                let _ = reply.send(database.memory_usage());
            }
            Request::Shutdown => break,
        }
    }
}

fn guarded_execution<T>(execute: impl FnOnce() -> T) -> Result<T, WorkerError> {
    catch_unwind(AssertUnwindSafe(execute)).map_err(|_| WorkerError::Panicked)
}

pub fn tooling_result(
    names: &[&str],
    types: Vec<koko::LogicalType>,
    rows: Vec<Vec<koko::Value>>,
) -> Result<QueryResult, WorkerError> {
    let columns = names
        .iter()
        .zip(types)
        .map(|(name, logical_type)| Column::new(*name, logical_type))
        .collect();
    Ok(koko::tooling::tabular_result(columns, rows)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_panic_identity_does_not_expose_payload() {
        let error = guarded_execution(|| panic!("private panic payload")).unwrap_err();
        assert!(matches!(error, WorkerError::Panicked));
        assert_eq!(error.to_string(), "internal CLI session worker failure");
        assert!(!error.to_string().contains("private panic payload"));
    }
}
