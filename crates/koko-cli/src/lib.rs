//! First-party Koko terminal client application core.

pub mod app_output;
pub mod bootstrap;
pub mod command;
mod continuation;
pub mod editor;
pub mod history;
pub mod human;
pub mod machine;
pub mod metadata;
pub mod output;
pub mod parameter;
pub mod platform;
pub mod presentation;
pub mod registry;
pub mod runner;
mod signal;
pub mod value_codec;
pub mod worker;

use app_output::OutputManager;
use bootstrap::{BootstrapAction, BootstrapError, InputMode};
use platform::RealProcess;
use runner::{SessionState, SourceRunner};
use std::ffi::OsString;
use std::io::Write;
use worker::SessionWorker;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitDecision {
    Success,
    RuntimeFailure,
    Usage,
    Interrupted,
}

impl ExitDecision {
    pub const fn code(self) -> u8 {
        match self {
            Self::Success => 0,
            Self::RuntimeFailure => 1,
            Self::Usage => 2,
            Self::Interrupted => 130,
        }
    }
}

/// Real-process entry. Engine activation is invoked only after bootstrap validation succeeds.
pub fn run_process<I, T>(arguments: I) -> ExitDecision
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    match bootstrap::validate(arguments, &RealProcess) {
        Ok(BootstrapAction::Print(text)) => write_bootstrap_text(&text),
        Ok(BootstrapAction::Activate(plan)) => activate(*plan),
        Err(error) => write_bootstrap_error(&error),
    }
}

fn write_bootstrap_text(text: &str) -> ExitDecision {
    match std::io::stdout().lock().write_all(text.as_bytes()) {
        Ok(()) => ExitDecision::Success,
        Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => ExitDecision::Success,
        Err(error) => {
            let _ = writeln!(std::io::stderr().lock(), "koko: {error}");
            ExitDecision::RuntimeFailure
        }
    }
}

fn write_bootstrap_error(error: &BootstrapError) -> ExitDecision {
    let _ = writeln!(std::io::stderr().lock(), "koko: {error}");
    ExitDecision::Usage
}

fn activate(plan: bootstrap::BootstrapPlan) -> ExitDecision {
    match activate_inner(plan) {
        Ok(decision) => decision,
        Err((decision, message)) => {
            let _ = writeln!(std::io::stderr().lock(), "koko: {message}");
            decision
        }
    }
}

fn activate_inner(plan: bootstrap::BootstrapPlan) -> Result<ExitDecision, (ExitDecision, String)> {
    let worker = SessionWorker::start()
        .map_err(|error| (ExitDecision::RuntimeFailure, error.to_string()))?;
    let mut state = SessionState::new(worker, plan.parameters, plan.startup_settings);

    if let Some(path) = &plan.init {
        let mut output = OutputManager::begin_silent(state.settings.clone())
            .map_err(|error| (ExitDecision::RuntimeFailure, error.to_string()))?;
        let init_result = {
            let mut runner = SourceRunner::new(&mut state, &mut output, false, false);
            runner.run_path(path, false)
        };
        let init_failed = init_result.as_ref().map_or(true, |summary| summary.failed);
        if let Err(error) = &init_result {
            let _ = output.diagnostic(&format!("koko: initialization failed: {error}"));
        }
        output
            .finish(!init_failed)
            .map_err(|error| (ExitDecision::RuntimeFailure, error.to_string()))?;
        if init_failed {
            rollback_if_active(&state);
            return Ok(ExitDecision::RuntimeFailure);
        }
    }
    state.settings.apply_command_line_overrides(&plan.settings);

    let mut output = OutputManager::begin(plan.output, plan.force, state.settings.clone(), None)
        .map_err(|error| (ExitDecision::RuntimeFailure, error.to_string()))?;
    let run = match &plan.input {
        InputMode::Command(source) => {
            let mut runner = SourceRunner::new(&mut state, &mut output, false, plan.keep_going);
            runner.run_text(source, "<command>")
        }
        InputMode::File(path) => {
            let mut runner = SourceRunner::new(&mut state, &mut output, false, plan.keep_going);
            runner.run_path(path, plan.keep_going)
        }
        InputMode::PipedStdin => {
            let stdin = std::io::stdin();
            let mut input = stdin.lock();
            let mut runner = SourceRunner::new(&mut state, &mut output, false, plan.keep_going);
            runner
                .run_reader(&mut input, "<stdin>", None, plan.keep_going)
                .map(|()| runner.summary())
        }
        InputMode::Interactive => editor::run_interactive(
            &mut state,
            &mut output,
            plan.quiet,
            RealProcess.user_history_file(),
        ),
    };

    let mut decision = match run {
        Ok(summary) if summary.interrupted => ExitDecision::Interrupted,
        Ok(summary) if summary.failed => ExitDecision::RuntimeFailure,
        Ok(_) => ExitDecision::Success,
        Err(error) => {
            let _ = output.diagnostic(&format!("koko: {error}"));
            if error.is_usage() {
                ExitDecision::Usage
            } else {
                ExitDecision::RuntimeFailure
            }
        }
    };
    if state
        .worker
        .session_snapshot()
        .is_ok_and(|session| session.transaction() != koko::TransactionMode::None)
    {
        let _ = output.diagnostic("Batch input ended with an active transaction; rolling it back.");
        rollback_if_active(&state);
        if decision != ExitDecision::Interrupted {
            decision = ExitDecision::RuntimeFailure;
        }
    }
    output
        .finish(decision == ExitDecision::Success)
        .map_err(|error| (ExitDecision::RuntimeFailure, error.to_string()))?;
    Ok(decision)
}

fn rollback_if_active(state: &SessionState) {
    if state
        .worker
        .session_snapshot()
        .is_ok_and(|session| session.transaction() != koko::TransactionMode::None)
    {
        let _ = state
            .worker
            .execute("ROLLBACK".to_string(), state.parameters.clone());
    }
}
