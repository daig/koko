//! Invocation-scoped presentation and atomic destination ownership.

use crate::bootstrap::{Format, OutputDestination, Settings};
use crate::command::{OutputCommand, OutputMode};
use crate::output::{CollisionPolicy, OutputError, OutputTransaction};
use crate::presentation::{PresentationError, PresentationSettings, Presenter, StatementContext};
use koko::QueryResult;
use koko::diagnostics::Failure;
use std::io::{self, Write};
use std::path::PathBuf;

pub enum DestinationWriter {
    Stdout(io::Stdout),
    File(OutputTransaction),
}

impl Write for DestinationWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        match self {
            Self::Stdout(writer) => writer.write(buffer),
            Self::File(writer) => writer.write(buffer),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Stdout(writer) => writer.flush(),
            Self::File(writer) => writer.flush(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AppOutputError {
    #[error(transparent)]
    Presentation(#[from] PresentationError),
    #[error(transparent)]
    Output(#[from] OutputError),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("cannot change format after writing to one atomic file destination")]
    FileFormatChange,
}

#[derive(Debug, Clone)]
enum DestinationSpec {
    Stdout {
        terminal: bool,
    },
    File {
        path: PathBuf,
        policy: CollisionPolicy,
    },
}

pub struct OutputManager {
    presenter: Option<Presenter<DestinationWriter, io::Stderr>>,
    destination: DestinationSpec,
    staged: Vec<OutputTransaction>,
    settings: Settings,
    terminal_width: Option<usize>,
    forced_format: Option<Format>,
    failed: bool,
    progress_active: bool,
    progress_terminal: bool,
}

impl OutputManager {
    pub fn begin(
        destination: OutputDestination,
        force: bool,
        settings: Settings,
        terminal_width: Option<usize>,
    ) -> Result<Self, AppOutputError> {
        Self::begin_inner(destination, force, settings, terminal_width, None)
    }

    pub fn begin_silent(settings: Settings) -> Result<Self, AppOutputError> {
        Self::begin_inner(
            OutputDestination::Stdout { terminal: false },
            false,
            settings,
            None,
            Some(Format::Trash),
        )
    }

    fn begin_inner(
        destination: OutputDestination,
        force: bool,
        settings: Settings,
        terminal_width: Option<usize>,
        forced_format: Option<Format>,
    ) -> Result<Self, AppOutputError> {
        let destination = match destination {
            OutputDestination::Stdout { terminal } => DestinationSpec::Stdout { terminal },
            OutputDestination::File(path) => DestinationSpec::File {
                path,
                policy: if force {
                    CollisionPolicy::Replace
                } else {
                    CollisionPolicy::Refuse
                },
            },
        };
        let presenter = Some(Self::make_presenter(
            &destination,
            &settings,
            terminal_width,
            forced_format,
        )?);
        Ok(Self {
            presenter,
            destination,
            staged: Vec::new(),
            settings,
            terminal_width,
            forced_format,
            failed: false,
            progress_active: false,
            progress_terminal: false,
        })
    }

    pub fn format(&self) -> Format {
        self.presenter.as_ref().expect("active presenter").format()
    }

    pub fn destination_name(&self) -> String {
        match &self.destination {
            DestinationSpec::Stdout { .. } => "stdout".to_string(),
            DestinationSpec::File { path, .. } => path.display().to_string(),
        }
    }

    pub fn present_result(
        &mut self,
        statement: &StatementContext<'_>,
        result: &QueryResult,
    ) -> Result<(), AppOutputError> {
        self.presenter
            .as_mut()
            .expect("active presenter")
            .present_result(statement, result)?;
        Ok(())
    }

    pub fn present_failure(
        &mut self,
        statement: &StatementContext<'_>,
        failure: &Failure,
    ) -> Result<(), AppOutputError> {
        self.failed = true;
        self.presenter
            .as_mut()
            .expect("active presenter")
            .present_failure(statement, failure)?;
        Ok(())
    }

    pub fn present_text(&mut self, text: &str) -> Result<(), AppOutputError> {
        self.presenter
            .as_mut()
            .expect("active presenter")
            .present_text(text)?;
        Ok(())
    }

    pub fn diagnostic(&mut self, message: &str) -> Result<(), AppOutputError> {
        self.presenter
            .as_mut()
            .expect("active presenter")
            .diagnostic(message)?;
        Ok(())
    }

    pub fn progress(
        &mut self,
        elapsed: std::time::Duration,
        cancelling: bool,
        terminal: bool,
    ) -> Result<(), AppOutputError> {
        let presenter = self.presenter.as_mut().expect("active presenter");
        if terminal {
            let status = if cancelling {
                "Cancelling…".to_string()
            } else {
                format!("Running… {:.1} s · Ctrl-C to cancel", elapsed.as_secs_f64())
            };
            presenter.transient_diagnostic(&format!("\r\u{1b}[2K{status}"))?;
            self.progress_active = true;
            self.progress_terminal = true;
        } else if !self.progress_active || cancelling {
            presenter.diagnostic(if cancelling {
                "Cancelling…"
            } else {
                "Running… Ctrl-C to cancel"
            })?;
            self.progress_active = true;
            self.progress_terminal = false;
        }
        Ok(())
    }

    pub fn clear_progress(&mut self) -> Result<(), AppOutputError> {
        if self.progress_active && self.progress_terminal {
            self.presenter
                .as_mut()
                .expect("active presenter")
                .transient_diagnostic("\r\u{1b}[2K\r")?;
        }
        self.progress_active = false;
        self.progress_terminal = false;
        Ok(())
    }

    pub fn apply_settings(&mut self, settings: Settings) -> Result<(), AppOutputError> {
        let terminal = matches!(self.destination, DestinationSpec::Stdout { terminal: true });
        let resolved =
            Self::resolve_settings(&settings, terminal, self.terminal_width, self.forced_format);
        let current_format = self.format();
        if resolved.format != current_format {
            if matches!(self.destination, DestinationSpec::File { .. }) {
                return Err(AppOutputError::FileFormatChange);
            }
            self.close_presenter()?;
            self.presenter = Some(Self::make_presenter(
                &self.destination,
                &settings,
                self.terminal_width,
                self.forced_format,
            )?);
        } else {
            self.presenter
                .as_mut()
                .expect("active presenter")
                .update_settings(resolved);
        }
        self.settings = settings;
        Ok(())
    }

    pub fn switch(&mut self, command: OutputCommand) -> Result<(), AppOutputError> {
        let destination = match command {
            OutputCommand::Current => return Ok(()),
            OutputCommand::Stdout => DestinationSpec::Stdout { terminal: false },
            OutputCommand::File { path, mode } => DestinationSpec::File {
                path,
                policy: match mode {
                    OutputMode::Refuse => CollisionPolicy::Refuse,
                    OutputMode::Append => CollisionPolicy::Append,
                    OutputMode::Replace => CollisionPolicy::Replace,
                },
            },
        };
        let presenter = Self::make_presenter(
            &destination,
            &self.settings,
            self.terminal_width,
            self.forced_format,
        )?;
        self.close_presenter()?;
        self.destination = destination;
        self.presenter = Some(presenter);
        Ok(())
    }

    pub fn finish(mut self, success: bool) -> Result<(), AppOutputError> {
        self.failed |= !success;
        if self.failed {
            self.presenter
                .as_mut()
                .expect("active presenter")
                .mark_failed();
        }
        self.close_presenter()?;
        if self.failed {
            return Ok(());
        }
        for transaction in self.staged {
            transaction.commit()?;
        }
        Ok(())
    }

    fn make_presenter(
        destination: &DestinationSpec,
        settings: &Settings,
        terminal_width: Option<usize>,
        forced_format: Option<Format>,
    ) -> Result<Presenter<DestinationWriter, io::Stderr>, AppOutputError> {
        let (writer, terminal) = match destination {
            DestinationSpec::Stdout { terminal } => {
                (DestinationWriter::Stdout(io::stdout()), *terminal)
            }
            DestinationSpec::File { path, policy } => {
                let presentation =
                    Self::resolve_settings(settings, false, terminal_width, forced_format);
                let writer = OutputTransaction::begin(path, *policy, presentation.format)?;
                (DestinationWriter::File(writer), false)
            }
        };
        let presentation =
            Self::resolve_settings(settings, terminal, terminal_width, forced_format);
        Ok(Presenter::begin(writer, io::stderr(), presentation)?)
    }

    fn resolve_settings(
        settings: &Settings,
        terminal: bool,
        terminal_width: Option<usize>,
        forced_format: Option<Format>,
    ) -> PresentationSettings {
        let mut presentation = PresentationSettings::resolve(settings, terminal, terminal_width);
        if let Some(format) = forced_format {
            presentation.format = format;
        }
        presentation
    }

    fn close_presenter(&mut self) -> Result<(), AppOutputError> {
        let Some(presenter) = self.presenter.take() else {
            return Ok(());
        };
        let (writer, _) = presenter.finish()?;
        if let DestinationWriter::File(transaction) = writer {
            self.staged.push(transaction);
        }
        Ok(())
    }
}
