//! Single-owner presentation state and bounded output protocols.

use crate::bootstrap::{Format, NullDisplay, RowLimit, Settings, WidthLimit};
use crate::{human, machine, output, value_codec};
use koko::{QueryResult, QueryResultKind, StatementFailure};
use std::io::Write;

#[derive(Debug, Clone, Copy)]
pub struct StatementContext<'a> {
    pub number: usize,
    pub result: usize,
    pub total_results: Option<usize>,
    pub source: Option<&'a str>,
    pub line: Option<u64>,
    pub column: Option<u64>,
}

impl StatementContext<'_> {
    pub const fn new(number: usize, result: usize) -> Self {
        Self {
            number,
            result,
            total_results: None,
            source: None,
            line: None,
            column: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct PresentationSettings {
    pub format: Format,
    pub timing: bool,
    pub header: bool,
    pub null_token: String,
    pub row_limit: Option<usize>,
    pub max_width: Option<usize>,
    pub null_display: String,
}

impl PresentationSettings {
    pub fn resolve(
        settings: &Settings,
        data_terminal: bool,
        terminal_width: Option<usize>,
    ) -> Self {
        let format = match settings.format.value() {
            Format::Auto if data_terminal && terminal_width.is_some() => Format::Box,
            Format::Auto if data_terminal => Format::Table,
            Format::Auto => Format::Tsv,
            format => *format,
        };
        let row_limit = match settings.rows.value() {
            RowLimit::Rows(limit) => Some(*limit),
            RowLimit::All => None,
        };
        let max_width = match settings.width.value() {
            WidthLimit::Auto if data_terminal => terminal_width,
            WidthLimit::Auto => None,
            WidthLimit::Columns(columns) => Some(*columns),
        };
        let null_display = match settings.null_display.value() {
            NullDisplay::Literal => "NULL",
            NullDisplay::Empty => "",
        };
        Self {
            format,
            timing: *settings.timing.value(),
            header: *settings.header.value(),
            null_token: settings.null_token.value().clone(),
            row_limit,
            max_width,
            null_display: null_display.to_string(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PresentationError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Engine(#[from] koko::Error),
    #[error(transparent)]
    Encode(#[from] value_codec::EncodeError),
    #[error("CSV/TSV output supports only one row-producing result per invocation")]
    AmbiguousDelimited,
    #[error("CSV NULL token `{0}` cannot be represented unquoted")]
    InvalidNullToken(String),
}

pub struct Presenter<W: Write, D: Write> {
    data: W,
    diagnostics: D,
    settings: PresentationSettings,
    json_first: bool,
    json_closed: bool,
    failed: bool,
    row_results: usize,
    delimited: Option<tempfile::SpooledTempFile>,
}

impl<W: Write, D: Write> Presenter<W, D> {
    pub fn begin(
        mut data: W,
        diagnostics: D,
        settings: PresentationSettings,
    ) -> Result<Self, PresentationError> {
        if settings.format == Format::Json {
            machine::begin_json(&mut data)?;
        }
        let delimited = matches!(settings.format, Format::Csv | Format::Tsv)
            .then(|| tempfile::spooled_tempfile(64 * 1024));
        Ok(Self {
            data,
            diagnostics,
            settings,
            json_first: true,
            json_closed: false,
            failed: false,
            row_results: 0,
            delimited,
        })
    }

    pub const fn format(&self) -> Format {
        self.settings.format
    }

    pub fn update_settings(&mut self, settings: PresentationSettings) {
        debug_assert_eq!(self.settings.format, settings.format);
        self.settings = settings;
    }

    pub fn mark_failed(&mut self) {
        self.failed = true;
    }

    pub fn present_text(&mut self, text: &str) -> Result<(), PresentationError> {
        self.data.write_all(text.as_bytes())?;
        if !text.ends_with('\n') {
            self.data.write_all(b"\n")?;
        }
        Ok(())
    }

    pub fn diagnostic(&mut self, message: &str) -> Result<(), PresentationError> {
        self.diagnostics.write_all(message.as_bytes())?;
        if !message.ends_with('\n') {
            self.diagnostics.write_all(b"\n")?;
        }
        Ok(())
    }

    pub fn transient_diagnostic(&mut self, text: &str) -> Result<(), PresentationError> {
        self.diagnostics.write_all(text.as_bytes())?;
        self.diagnostics.flush()?;
        Ok(())
    }

    pub fn present_result(
        &mut self,
        statement: &StatementContext<'_>,
        result: &QueryResult,
    ) -> Result<(), PresentationError> {
        self.write_warnings(result)?;
        let mut displayed_rows = None;
        if is_human(self.settings.format) && statement.total_results.is_some_and(|total| total > 1)
        {
            writeln!(
                self.data,
                "Result {} of {}{}",
                statement.result,
                statement.total_results.unwrap_or(statement.result),
                statement
                    .source
                    .map(|source| format!(" ({source})"))
                    .unwrap_or_default()
            )?;
        }
        match self.settings.format {
            Format::Json => machine::write_json_result(
                &mut self.data,
                statement,
                result,
                &mut self.json_first,
                self.settings.timing,
            )?,
            Format::JsonLines => machine::write_jsonl_result(
                &mut self.data,
                statement,
                result,
                self.settings.timing,
            )?,
            Format::Csv | Format::Tsv if machine::is_row_producing(result) => {
                self.row_results += 1;
                if self.row_results > 1 {
                    self.failed = true;
                    return Err(PresentationError::AmbiguousDelimited);
                }
                let delimiter = if self.settings.format == Format::Csv {
                    b','
                } else {
                    b'\t'
                };
                machine::write_delimited(
                    self.delimited.as_mut().expect("delimited spill"),
                    result,
                    delimiter,
                    self.settings.header,
                    &self.settings.null_token,
                )?;
            }
            Format::Csv | Format::Tsv | Format::Trash => {}
            format => {
                displayed_rows = Some(human::write_result(
                    &mut self.data,
                    result,
                    &human::HumanOptions {
                        format,
                        row_limit: self.settings.row_limit,
                        max_width: self.settings.max_width,
                        null_display: self.settings.null_display.clone(),
                    },
                )?);
            }
        }
        if !matches!(self.settings.format, Format::Json | Format::JsonLines) {
            if let Some(status) = result.status_message() {
                writeln!(self.diagnostics, "{status}")?;
            }
            if result.result_kind() == QueryResultKind::Rows {
                write!(self.diagnostics, "{} rows returned", result.num_rows())?;
                if displayed_rows.is_some_and(|displayed| displayed != result.num_rows()) {
                    write!(
                        self.diagnostics,
                        "; {} displayed",
                        displayed_rows.unwrap_or(result.num_rows())
                    )?;
                }
                self.diagnostics.write_all(b"\n")?;
            }
        }
        if self.settings.timing && !matches!(self.settings.format, Format::Json | Format::JsonLines)
        {
            writeln!(
                self.diagnostics,
                "Timing: {:.3} ms compiling, {:.3} ms executing",
                result.summary().compiling_time_ms(),
                result.summary().execution_time_ms()
            )?;
        }
        Ok(())
    }

    pub fn present_failure(
        &mut self,
        statement: &StatementContext<'_>,
        failure: &StatementFailure,
    ) -> Result<(), PresentationError> {
        self.failed = true;
        let display = if failure.kind() == koko::FailureKind::InternalPanic {
            "Internal error: query execution panicked.".to_string()
        } else {
            failure.error().to_string()
        };
        writeln!(self.diagnostics, "{display}")?;
        match self.settings.format {
            Format::Json => {
                machine::finish_json(&mut self.data, false, Some((statement, failure)))?;
                self.json_closed = true;
            }
            Format::JsonLines => machine::write_jsonl_error(&mut self.data, statement, failure)?,
            _ => {}
        }
        Ok(())
    }

    pub fn finish(mut self) -> Result<(W, D), PresentationError> {
        if self.settings.format == Format::Json && !self.json_closed {
            machine::finish_json(&mut self.data, !self.failed, None)?;
        }
        if matches!(self.settings.format, Format::Csv | Format::Tsv) && !self.failed {
            output::publish_spill(
                self.delimited.as_mut().expect("delimited spill"),
                &mut self.data,
            )?;
        }
        self.data.flush()?;
        self.diagnostics.flush()?;
        Ok((self.data, self.diagnostics))
    }

    fn write_warnings(&mut self, result: &QueryResult) -> Result<(), PresentationError> {
        for warning in result.statement_diagnostics().warnings() {
            writeln!(self.diagnostics, "Warning: {}", warning.message())?;
        }
        Ok(())
    }
}

fn is_human(format: Format) -> bool {
    matches!(
        format,
        Format::Box | Format::Table | Format::Markdown | Format::Line
    )
}
