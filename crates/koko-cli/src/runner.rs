//! One ordered source/session runner for command, file, stdin, and includes.

use crate::app_output::{AppOutputError, OutputManager};
use crate::bootstrap::{
    AutoToggle, Format, NullDisplay, Resolved, RowLimit, SettingSource, Settings, WidthLimit,
};
use crate::command::{CommandError, HistoryAction, MetaCommand, OutputCommand, parse_meta_command};
use crate::history::{HistoryController, HistoryError};
use crate::metadata;
use crate::parameter::{ParameterError, ParameterStore};
use crate::presentation::StatementContext;
use crate::registry::CommandId;
use crate::worker::{ExecutionEvent, SessionWorker, WorkerError, tooling_result};
use koko::{
    FailureKind, InterruptReason, LogicalType, OutputClass, QueryResult, QueryResultKind,
    StatementClass, SyntaxStatus, TransactionMode, Value, analyze_cypher,
};
use std::fs::File;
use std::io::{self, BufRead, BufReader, Cursor, IsTerminal};
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum RunnerError {
    #[error(transparent)]
    Command(#[from] CommandError),
    #[error(transparent)]
    Parameter(#[from] ParameterError),
    #[error(transparent)]
    Worker(#[from] WorkerError),
    #[error(transparent)]
    Output(#[from] AppOutputError),
    #[error(transparent)]
    History(#[from] HistoryError),
    #[error("terminal I/O failed: {0}")]
    Terminal(#[from] io::Error),
    #[error("interactive editor failed: {0}")]
    Editor(String),
    #[error("cannot read `{path}`: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("include cycle: {0}")]
    IncludeCycle(String),
    #[error("invalid value `{value}` for :{command}; expected {expected}")]
    Setting {
        command: &'static str,
        value: String,
        expected: &'static str,
    },
}

impl RunnerError {
    pub const fn is_usage(&self) -> bool {
        matches!(
            self,
            Self::Command(_) | Self::Parameter(_) | Self::Setting { .. }
        )
    }

    pub const fn is_interactive_recoverable(&self) -> bool {
        matches!(
            self,
            Self::Command(_)
                | Self::Parameter(_)
                | Self::Read { .. }
                | Self::IncludeCycle(_)
                | Self::Setting { .. }
        )
    }
}

pub struct SessionState {
    pub worker: SessionWorker,
    pub parameters: ParameterStore,
    pub settings: Settings,
    statement_number: usize,
}

impl SessionState {
    pub fn new(worker: SessionWorker, parameters: ParameterStore, settings: Settings) -> Self {
        Self {
            worker,
            parameters,
            settings,
            statement_number: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct RunSummary {
    pub failed: bool,
    pub quit: bool,
    pub interrupted: bool,
}

pub struct SourceRunner<'a> {
    state: &'a mut SessionState,
    output: &'a mut OutputManager,
    interactive: bool,
    keep_going: bool,
    include_stack: Vec<PathBuf>,
    summary: RunSummary,
    history: Option<HistoryController>,
    halt: bool,
    submission_row: usize,
    submission_total_rows: Option<usize>,
}

impl<'a> SourceRunner<'a> {
    pub fn new(
        state: &'a mut SessionState,
        output: &'a mut OutputManager,
        interactive: bool,
        keep_going: bool,
    ) -> Self {
        Self {
            state,
            output,
            interactive,
            keep_going,
            include_stack: Vec::new(),
            summary: RunSummary::default(),
            history: None,
            halt: false,
            submission_row: 0,
            submission_total_rows: None,
        }
    }

    pub const fn summary(&self) -> RunSummary {
        self.summary
    }

    pub fn with_history(mut self, history: HistoryController) -> Self {
        self.history = Some(history);
        self
    }

    pub fn settings(&self) -> &Settings {
        &self.state.settings
    }

    pub fn parameters(&self) -> &ParameterStore {
        &self.state.parameters
    }

    pub fn session_snapshot(&self) -> Result<koko::SessionSnapshot, RunnerError> {
        Ok(self.state.worker.session_snapshot()?)
    }

    pub fn catalog_snapshot(&self) -> Result<koko::CatalogSnapshot, RunnerError> {
        Ok(self.state.worker.catalog_snapshot()?)
    }

    pub fn run_interactive_input(&mut self, source: &str) -> Result<RunSummary, RunnerError> {
        self.halt = false;
        self.summary.failed = false;
        self.summary.interrupted = false;
        self.begin_submission(source);
        let mut reader = Cursor::new(source.as_bytes());
        let result = self.run_reader(&mut reader, "<interactive>", None, true);
        self.end_submission();
        result?;
        Ok(self.summary)
    }

    pub fn request_exit(&mut self) -> Result<bool, RunnerError> {
        self.quit(false)?;
        Ok(self.summary.quit)
    }

    pub fn diagnostic(&mut self, message: &str) -> Result<(), RunnerError> {
        self.output.diagnostic(message)?;
        Ok(())
    }

    pub fn run_text(&mut self, source: &str, label: &str) -> Result<RunSummary, RunnerError> {
        self.begin_submission(source);
        let mut reader = Cursor::new(source.as_bytes());
        let result = self.run_reader(&mut reader, label, None, self.keep_going);
        self.end_submission();
        result?;
        Ok(self.summary)
    }

    fn begin_submission(&mut self, source: &str) {
        let rows = analyze_cypher(source, None)
            .statements()
            .iter()
            .filter(|statement| statement.output_class() == Some(OutputClass::Rows))
            .count();
        self.submission_row = 0;
        self.submission_total_rows = (rows > 1).then_some(rows);
    }

    fn end_submission(&mut self) {
        self.submission_row = 0;
        self.submission_total_rows = None;
    }

    pub fn run_path(&mut self, path: &Path, keep_going: bool) -> Result<RunSummary, RunnerError> {
        let path = canonical_local(path).map_err(|source| RunnerError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        if let Some(position) = self.include_stack.iter().position(|item| item == &path) {
            let mut cycle = self.include_stack[position..]
                .iter()
                .map(|item| item.display().to_string())
                .collect::<Vec<_>>();
            cycle.push(path.display().to_string());
            return Err(RunnerError::IncludeCycle(cycle.join(" -> ")));
        }
        let file = File::open(&path).map_err(|source| RunnerError::Read {
            path: path.clone(),
            source,
        })?;
        self.include_stack.push(path.clone());
        let base = path.parent().map(Path::to_path_buf);
        let result = self.run_reader(
            &mut BufReader::new(file),
            &path.display().to_string(),
            base.as_deref(),
            keep_going,
        );
        self.include_stack.pop();
        result?;
        Ok(self.summary)
    }

    pub fn run_reader(
        &mut self,
        reader: &mut dyn BufRead,
        label: &str,
        base: Option<&Path>,
        keep_going: bool,
    ) -> Result<(), RunnerError> {
        let mut pending = String::new();
        let mut pending_line = 1_u64;
        let mut physical_line = 0_u64;
        let mut line = String::new();
        loop {
            line.clear();
            let bytes = reader
                .read_line(&mut line)
                .map_err(|source| RunnerError::Read {
                    path: PathBuf::from(label),
                    source,
                })?;
            if bytes == 0 {
                break;
            }
            physical_line += 1;
            if pending.is_empty() && line.trim_start().starts_with(':') {
                self.run_command(line.trim(), base, keep_going)?;
                if self.summary.quit || self.halt {
                    return Ok(());
                }
                continue;
            }
            if pending.is_empty() {
                pending_line = physical_line;
            }
            pending.push_str(&line);
            self.execute_terminated(&mut pending, &mut pending_line, label, keep_going)?;
            if self.summary.quit || self.halt || (self.summary.failed && !keep_going) {
                return Ok(());
            }
        }
        if !pending.is_empty() && analyze_cypher(&pending, None).status() != SyntaxStatus::Empty {
            self.execute_statement(&pending, label, pending_line, 1, None, keep_going)?;
        }
        Ok(())
    }

    fn execute_terminated(
        &mut self,
        pending: &mut String,
        pending_line: &mut u64,
        label: &str,
        keep_going: bool,
    ) -> Result<(), RunnerError> {
        let analysis = analyze_cypher(pending, None);
        if analysis.status() == SyntaxStatus::Empty {
            *pending_line += pending.bytes().filter(|byte| *byte == b'\n').count() as u64;
            pending.clear();
            return Ok(());
        }
        let mut consumed = 0;
        for statement in analysis.statements() {
            let span = statement.span();
            let suffix = &pending[span.end()..];
            let Some(relative_semicolon) = suffix.find(';') else {
                break;
            };
            if !suffix[..relative_semicolon].trim().is_empty() {
                break;
            }
            let before = &pending[..span.start()];
            let line_offset = before.bytes().filter(|byte| *byte == b'\n').count() as u64;
            let column = before
                .rsplit_once('\n')
                .map_or(span.start() + 1, |(_, tail)| tail.len() + 1);
            self.execute_statement(
                &pending[span.start()..span.end()],
                label,
                *pending_line + line_offset,
                column as u64,
                statement.class(),
                keep_going,
            )?;
            consumed = span.end() + relative_semicolon + 1;
            if self.summary.quit || self.halt || (self.summary.failed && !keep_going) {
                break;
            }
        }
        if consumed != 0 {
            let lines = pending[..consumed]
                .bytes()
                .filter(|byte| *byte == b'\n')
                .count() as u64;
            pending.drain(..consumed);
            *pending_line += lines;
            if analyze_cypher(pending, None).status() == SyntaxStatus::Empty {
                *pending_line += pending.bytes().filter(|byte| *byte == b'\n').count() as u64;
                pending.clear();
            }
        }
        Ok(())
    }

    fn execute_statement(
        &mut self,
        cypher: &str,
        label: &str,
        line: u64,
        column: u64,
        class: Option<StatementClass>,
        keep_going: bool,
    ) -> Result<(), RunnerError> {
        self.state.statement_number += 1;
        let number = self.state.statement_number;

        let progress_enabled = match self.state.settings.progress.value() {
            AutoToggle::On => true,
            AutoToggle::Off => false,
            AutoToggle::Auto => self.interactive && std::io::stderr().is_terminal(),
        };
        let progress_terminal = std::io::stderr().is_terminal()
            && !std::env::var("TERM").is_ok_and(|term| term.eq_ignore_ascii_case("dumb"));
        let execution = {
            let output = &mut self.output;
            self.state.worker.execute_observed(
                cypher.to_string(),
                self.state.parameters.clone(),
                |event| match event {
                    ExecutionEvent::Progress(elapsed) if progress_enabled => output
                        .progress(elapsed, false, progress_terminal)
                        .map_err(|error| error.to_string()),
                    ExecutionEvent::CancellationRequested(elapsed) => output
                        .progress(elapsed, true, progress_terminal)
                        .map_err(|error| error.to_string()),
                    ExecutionEvent::Progress(_) => Ok(()),
                },
            )
        };
        let clear_result = self.output.clear_progress();
        let execution = execution?;
        clear_result?;
        let outcome = execution.outcome;
        if let Some(result) = outcome.result() {
            let (result_number, total_results) = if result.result_kind() == QueryResultKind::Rows {
                self.submission_row += 1;
                (
                    self.submission_total_rows
                        .map_or(number, |_| self.submission_row),
                    self.submission_total_rows,
                )
            } else {
                (number, None)
            };
            let context = StatementContext {
                number,
                result: result_number,
                total_results,
                source: Some(label),
                line: Some(line),
                column: Some(column),
            };
            self.output.present_result(&context, result)?;
            return Ok(());
        }
        let context = StatementContext {
            number,
            result: number,
            total_results: None,
            source: Some(label),
            line: Some(line),
            column: Some(column),
        };
        let failure = outcome
            .failure()
            .expect("structured outcome has result or failure");
        let explicitly_interrupted = failure.interrupt_reason() == Some(InterruptReason::Explicit);
        self.output.present_failure(&context, failure)?;
        if let Some(detail) = failure_detail(
            failure.kind(),
            failure.interrupt_reason(),
            execution.elapsed,
        ) {
            self.output.diagnostic(&detail)?;
        }
        if explicitly_interrupted {
            self.summary.interrupted = true;
        }
        self.summary.failed = true;
        if explicitly_interrupted {
            self.halt = true;
            return Ok(());
        }
        let transaction_active = outcome
            .session_before()
            .into_iter()
            .chain(outcome.session_after())
            .any(|session| session.transaction() != TransactionMode::None);
        if !keep_going || transaction_active || class == Some(StatementClass::Transaction) {
            self.halt = true;
            return Ok(());
        }
        Ok(())
    }

    fn run_command(
        &mut self,
        source: &str,
        base: Option<&Path>,
        inherited_keep_going: bool,
    ) -> Result<(), RunnerError> {
        let command = parse_meta_command(source, self.interactive)?;
        match command {
            MetaCommand::Help(topic) => self.emit_text(&help_text(topic.as_deref()))?,
            MetaCommand::Quit { rollback } => self.quit(rollback)?,
            MetaCommand::Clear => self.output.present_text("\u{1b}[2J\u{1b}[H")?,
            MetaCommand::Status => {
                let session = self.state.worker.session_snapshot()?;
                let memory = self.state.worker.memory_usage()?;
                let result = metadata::status_result(
                    &session,
                    memory,
                    &self.state.parameters,
                    &self.state.settings,
                    &self.output.destination_name(),
                )?;
                self.present_meta(result)?;
            }
            MetaCommand::Graphs => {
                let catalog = self.state.worker.catalog_snapshot()?;
                self.present_meta(metadata::graphs_result(&catalog)?)?;
            }
            MetaCommand::Schema(target) => self.schema(target.as_deref())?,
            MetaCommand::Describe(target) => {
                let catalog = if let Some((graph, _)) = target.split_once('.') {
                    self.state.worker.catalog_snapshot_for_graph(graph)?
                } else {
                    self.state.worker.catalog_snapshot()?
                };
                let result = metadata::describe_result(&catalog, &target)?;
                if result.num_rows() == 0 {
                    self.output
                        .diagnostic(&format!("No visible schema object matches `{target}`."))?;
                    self.summary.failed = true;
                } else {
                    self.present_meta(result)?;
                }
            }
            MetaCommand::Functions(pattern) => {
                let catalog = self.state.worker.catalog_snapshot()?;
                self.present_meta(metadata::functions_result(&catalog, pattern.as_deref())?)?;
            }
            MetaCommand::Parameters { values } => {
                self.present_meta(metadata::parameters_result(&self.state.parameters, values)?)?;
            }
            MetaCommand::ParameterSet { name, json } => {
                self.state.parameters.insert_interactive(&name, &json)?;
                self.emit_text(&format!("Parameter ${name} set."))?;
            }
            MetaCommand::ParameterClear(target) => {
                if let Some(name) = target {
                    if self.state.parameters.remove(&name) {
                        self.emit_text(&format!("Parameter ${name} cleared."))?;
                    } else {
                        self.output
                            .diagnostic(&format!("Parameter ${name} is not set."))?;
                    }
                } else {
                    self.state.parameters.clear();
                    self.emit_text("All parameters cleared.")?;
                }
            }
            MetaCommand::Setting { id, value } => self.setting(id, value.as_deref())?,
            MetaCommand::History(action) => self.history(action)?,
            MetaCommand::Read { path, keep_going } => {
                let path = if path.is_absolute() {
                    path
                } else {
                    base.unwrap_or_else(|| Path::new(".")).join(path)
                };
                self.run_path(&path, keep_going || inherited_keep_going)?;
            }
            MetaCommand::Output(command) => {
                if let OutputCommand::Current = command {
                    self.emit_text(&format!("output {}", self.output.destination_name()))?;
                } else {
                    self.output.switch(command)?;
                }
            }
        }
        Ok(())
    }

    fn present_meta(&mut self, result: QueryResult) -> Result<(), RunnerError> {
        self.state.statement_number += 1;
        let context =
            StatementContext::new(self.state.statement_number, self.state.statement_number);
        self.output.present_result(&context, &result)?;
        Ok(())
    }

    fn emit_text(&mut self, text: &str) -> Result<(), RunnerError> {
        if is_human(self.output.format()) {
            self.output.present_text(text)?;
        } else {
            let result = tooling_result(
                &["message"],
                vec![LogicalType::String],
                vec![vec![Value::String(text.to_string())]],
            )?;
            self.present_meta(result)?;
        }
        Ok(())
    }

    fn schema(&mut self, target: Option<&str>) -> Result<(), RunnerError> {
        let selected_catalog = self.state.worker.catalog_snapshot()?;
        let selected = selected_catalog
            .graphs()
            .iter()
            .find(|graph| graph.identity() == selected_catalog.selected_graph())
            .map(|graph| graph.name())
            .unwrap_or("main");
        let (graph, object) = match target {
            Some(target) if target.contains('.') => {
                let (graph, object) = target.split_once('.').expect("checked separator");
                (graph, Some(object))
            }
            Some(target)
                if selected_catalog
                    .graphs()
                    .iter()
                    .any(|graph| graph.name().eq_ignore_ascii_case(target)) =>
            {
                (target, None)
            }
            Some(target) => (selected, Some(target)),
            None => (selected, None),
        };
        let catalog = if graph.eq_ignore_ascii_case(selected) {
            selected_catalog
        } else {
            self.state.worker.catalog_snapshot_for_graph(graph)?
        };
        let statements = object.map_or_else(
            || catalog.schema_statements().collect::<Vec<_>>(),
            |object| catalog.schema_statements_for_object(object),
        );
        if object.is_some() && statements.is_empty() {
            self.output.diagnostic(&format!(
                "No visible schema object matches `{}`.",
                target.unwrap_or_default()
            ))?;
            self.summary.failed = true;
            return Ok(());
        }
        if is_human(self.output.format()) {
            self.output.present_text(&statements.join("\n"))?;
        } else {
            let rows = statements
                .into_iter()
                .map(|statement| vec![Value::String(statement.to_string())])
                .collect();
            self.present_meta(tooling_result(
                &["statement"],
                vec![LogicalType::String],
                rows,
            )?)?;
        }
        Ok(())
    }

    fn quit(&mut self, rollback: bool) -> Result<(), RunnerError> {
        let session = self.state.worker.session_snapshot()?;
        if session.transaction() == TransactionMode::None {
            self.summary.quit = true;
            return Ok(());
        }
        if !rollback {
            self.output
                .diagnostic("Transaction is active. COMMIT, ROLLBACK, or use :quit --rollback.")?;
            return Ok(());
        }
        self.execute_statement(
            "ROLLBACK",
            "<quit>",
            1,
            1,
            Some(StatementClass::Transaction),
            false,
        )?;
        if self.state.worker.session_snapshot()?.transaction() == TransactionMode::None {
            self.summary.quit = true;
        }
        Ok(())
    }

    fn setting(&mut self, id: CommandId, value: Option<&str>) -> Result<(), RunnerError> {
        if value.is_none() {
            self.emit_text(&setting_status(id, &self.state.settings))?;
            return Ok(());
        }
        let value = value.unwrap_or_default().to_ascii_lowercase();
        let source = SettingSource::Session;
        match id {
            CommandId::Format => replace_parsed(
                &mut self.state.settings.format,
                Format::parse(&value),
                source,
                "format",
                &value,
                "auto/box/table/csv/tsv/json/jsonl/markdown/line/trash",
            )?,
            CommandId::Timing => replace_parsed(
                &mut self.state.settings.timing,
                parse_bool(&value),
                source,
                "timing",
                &value,
                "on/off",
            )?,
            CommandId::Progress => replace_parsed(
                &mut self.state.settings.progress,
                parse_auto(&value),
                source,
                "progress",
                &value,
                "auto/on/off",
            )?,
            CommandId::Rows => {
                let parsed = match value.as_str() {
                    "all" => Some(RowLimit::All),
                    "default" => Some(RowLimit::Rows(20)),
                    _ => value
                        .parse::<usize>()
                        .ok()
                        .filter(|count| *count > 0)
                        .map(RowLimit::Rows),
                };
                replace_parsed(
                    &mut self.state.settings.rows,
                    parsed,
                    source,
                    "rows",
                    &value,
                    "positive number/all/default",
                )?;
            }
            CommandId::Width => {
                let parsed = if value == "auto" {
                    Some(WidthLimit::Auto)
                } else {
                    value
                        .parse::<usize>()
                        .ok()
                        .filter(|count| *count > 0)
                        .map(WidthLimit::Columns)
                };
                replace_parsed(
                    &mut self.state.settings.width,
                    parsed,
                    source,
                    "width",
                    &value,
                    "positive number/auto",
                )?;
            }
            CommandId::Null => replace_parsed(
                &mut self.state.settings.null_display,
                match value.as_str() {
                    "literal" => Some(NullDisplay::Literal),
                    "empty" => Some(NullDisplay::Empty),
                    _ => None,
                },
                source,
                "null",
                &value,
                "literal/empty",
            )?,
            CommandId::Multiline => replace_parsed(
                &mut self.state.settings.multiline,
                parse_bool(&value),
                source,
                "multiline",
                &value,
                "on/off",
            )?,
            CommandId::Highlight => replace_parsed(
                &mut self.state.settings.highlight,
                parse_auto(&value),
                source,
                "highlight",
                &value,
                "auto/on/off",
            )?,
            CommandId::Completion => replace_parsed(
                &mut self.state.settings.completion,
                parse_bool(&value),
                source,
                "completion",
                &value,
                "on/off",
            )?,
            _ => unreachable!("only setting commands route here"),
        }
        self.output.apply_settings(self.state.settings.clone())?;
        self.emit_text(&setting_status(id, &self.state.settings))?;
        Ok(())
    }

    fn history(&mut self, action: HistoryAction) -> Result<(), RunnerError> {
        let Some(history) = self.history.clone() else {
            self.output
                .diagnostic("Persistent history is unavailable in this input mode.")?;
            return Ok(());
        };
        match action {
            HistoryAction::Current => self.emit_text(&format!(
                "history {}",
                if history.enabled() { "on" } else { "off" }
            ))?,
            HistoryAction::Show(count) => {
                let entries = history.newest(count)?;
                if entries.is_empty() {
                    self.emit_text("No history entries.")?;
                } else {
                    let text = entries
                        .iter()
                        .enumerate()
                        .map(|(index, entry)| format!("{}  {entry}", index + 1))
                        .collect::<Vec<_>>()
                        .join("\n");
                    self.emit_text(&text)?;
                }
            }
            HistoryAction::Clear => {
                if history.take_clear_confirmation() {
                    history.clear()?;
                    self.emit_text("History cleared.")?;
                } else {
                    self.output.diagnostic(
                        "History was not cleared; interactive confirmation is required.",
                    )?;
                }
            }
            HistoryAction::On => {
                let enabled = history.set_enabled(true);
                self.state
                    .settings
                    .history
                    .replace(enabled, SettingSource::Session);
                self.emit_text(if enabled { "history on" } else { "history off" })?;
            }
            HistoryAction::Off => {
                history.set_enabled(false);
                self.state
                    .settings
                    .history
                    .replace(false, SettingSource::Session);
                self.emit_text("history off")?;
            }
            HistoryAction::Skip => {
                history.skip_next();
                self.emit_text("The next entry will not be stored in history.")?;
            }
        }
        Ok(())
    }
}

fn failure_detail(
    kind: FailureKind,
    reason: Option<InterruptReason>,
    elapsed: std::time::Duration,
) -> Option<String> {
    match reason {
        Some(InterruptReason::Explicit) => Some(format!(
            "Query cancelled after {:.1} s.",
            elapsed.as_secs_f64()
        )),
        Some(InterruptReason::Deadline) => Some(format!(
            "Query deadline expired after {:.1} s.",
            elapsed.as_secs_f64()
        )),
        None if kind == FailureKind::Memory => {
            Some("Query stopped at the tracked-memory limit.".to_string())
        }
        None => None,
    }
}

fn canonical_local(path: &Path) -> io::Result<PathBuf> {
    path.canonicalize()
}

fn replace_parsed<T>(
    setting: &mut Resolved<T>,
    parsed: Option<T>,
    source: SettingSource,
    command: &'static str,
    value: &str,
    expected: &'static str,
) -> Result<(), RunnerError> {
    let parsed = parsed.ok_or_else(|| RunnerError::Setting {
        command,
        value: value.to_string(),
        expected,
    })?;
    setting.replace(parsed, source);
    Ok(())
}

fn parse_bool(value: &str) -> Option<bool> {
    match value {
        "on" => Some(true),
        "off" => Some(false),
        _ => None,
    }
}

fn parse_auto(value: &str) -> Option<AutoToggle> {
    match value {
        "auto" => Some(AutoToggle::Auto),
        "on" => Some(AutoToggle::On),
        "off" => Some(AutoToggle::Off),
        _ => None,
    }
}

fn setting_status(id: CommandId, settings: &Settings) -> String {
    match id {
        CommandId::Format => format!(
            "format {:?} (auto/box/table/csv/tsv/json/jsonl/markdown/line/trash)",
            settings.format.value()
        )
        .to_ascii_lowercase(),
        CommandId::Timing => format!(
            "timing {} (on/off)",
            if *settings.timing.value() {
                "on"
            } else {
                "off"
            }
        ),
        CommandId::Progress => {
            format!("progress {:?} (auto/on/off)", settings.progress.value()).to_ascii_lowercase()
        }
        CommandId::Rows => format!(
            "rows {:?} (positive number/all/default)",
            settings.rows.value()
        )
        .to_ascii_lowercase(),
        CommandId::Width => format!("width {:?} (positive number/auto)", settings.width.value())
            .to_ascii_lowercase(),
        CommandId::Null => {
            format!("null {:?} (literal/empty)", settings.null_display.value()).to_ascii_lowercase()
        }
        CommandId::Multiline => format!(
            "multiline {} (on/off)",
            if *settings.multiline.value() {
                "on"
            } else {
                "off"
            }
        ),
        CommandId::Highlight => {
            format!("highlight {:?} (auto/on/off)", settings.highlight.value()).to_ascii_lowercase()
        }
        CommandId::Completion => format!(
            "completion {} (on/off)",
            if *settings.completion.value() {
                "on"
            } else {
                "off"
            }
        ),
        _ => unreachable!("setting status only"),
    }
}

fn is_human(format: Format) -> bool {
    matches!(
        format,
        Format::Box | Format::Table | Format::Markdown | Format::Line
    )
}

fn help_text(topic: Option<&str>) -> String {
    match topic {
        None => "Koko live commands\n  inspect: :status :graphs :schema :describe :functions\n  parameters: :params :param\n  display: :format :timing :progress :rows :width :null\n  input: :read :output :history :completion :highlight :multiline\n  session: :quit\nTopics: queries keys completion history formats parameters batch graphs transactions cancellation".to_string(),
        Some(topic) => format!("Help for {topic}. Use :help for the command and topic index."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn structured_failure_causes_have_distinct_human_details() {
        let elapsed = std::time::Duration::from_millis(2_400);
        let cancelled = failure_detail(
            FailureKind::Interrupt,
            Some(InterruptReason::Explicit),
            elapsed,
        )
        .unwrap();
        let deadline = failure_detail(
            FailureKind::Interrupt,
            Some(InterruptReason::Deadline),
            elapsed,
        )
        .unwrap();
        let memory = failure_detail(FailureKind::Memory, None, elapsed).unwrap();
        assert_eq!(cancelled, "Query cancelled after 2.4 s.");
        assert_eq!(deadline, "Query deadline expired after 2.4 s.");
        assert_eq!(memory, "Query stopped at the tracked-memory limit.");
        assert!(failure_detail(FailureKind::Runtime, None, elapsed).is_none());
    }
}
