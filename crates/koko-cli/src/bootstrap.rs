//! Validation-only startup, typed configuration, precedence, and mode selection.

use crate::parameter::{ParameterError, ParameterStore};
use crate::registry::{OPTION_REGISTRY, OptionId, OptionSpec, clap_command};
use clap::error::ErrorKind;
use koko::tooling::version;
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Auto,
    Box,
    Table,
    Csv,
    Tsv,
    Json,
    JsonLines,
    Markdown,
    Line,
    Trash,
}

impl Format {
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "auto" => Self::Auto,
            "box" => Self::Box,
            "table" => Self::Table,
            "csv" => Self::Csv,
            "tsv" => Self::Tsv,
            "json" => Self::Json,
            "jsonl" => Self::JsonLines,
            "markdown" => Self::Markdown,
            "line" => Self::Line,
            "trash" => Self::Trash,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoToggle {
    Auto,
    On,
    Off,
}

impl AutoToggle {
    fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "auto" => Self::Auto,
            "on" => Self::On,
            "off" => Self::Off,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorChoice {
    Auto,
    Always,
    Never,
}

impl ColorChoice {
    fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "auto" => Self::Auto,
            "always" => Self::Always,
            "never" => Self::Never,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowLimit {
    Rows(usize),
    All,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WidthLimit {
    Auto,
    Columns(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NullDisplay {
    Literal,
    Empty,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingSource {
    Default,
    Config { path: PathBuf, line: usize },
    CommandLine,
    Session,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved<T> {
    value: T,
    source: SettingSource,
}

impl<T> Resolved<T> {
    pub const fn value(&self) -> &T {
        &self.value
    }

    pub const fn source(&self) -> &SettingSource {
        &self.source
    }

    pub(crate) fn replace(&mut self, value: T, source: SettingSource) {
        self.value = value;
        self.source = source;
    }
}

#[derive(Debug, Clone)]
pub struct Settings {
    pub format: Resolved<Format>,
    pub timing: Resolved<bool>,
    pub progress: Resolved<AutoToggle>,
    pub color: Resolved<ColorChoice>,
    pub rows: Resolved<RowLimit>,
    pub width: Resolved<WidthLimit>,
    pub null_display: Resolved<NullDisplay>,
    pub multiline: Resolved<bool>,
    pub highlight: Resolved<AutoToggle>,
    pub completion: Resolved<bool>,
    pub history: Resolved<bool>,
    pub history_limit: Resolved<usize>,
    pub header: Resolved<bool>,
    pub null_token: Resolved<String>,
}

impl Settings {
    pub(crate) fn defaults(interactive: bool) -> Self {
        let default = SettingSource::Default;
        Self {
            format: resolved(Format::Auto, &default),
            timing: resolved(interactive, &default),
            progress: resolved(AutoToggle::Auto, &default),
            color: resolved(ColorChoice::Auto, &default),
            rows: resolved(RowLimit::Rows(20), &default),
            width: resolved(WidthLimit::Auto, &default),
            null_display: resolved(NullDisplay::Literal, &default),
            multiline: resolved(true, &default),
            highlight: resolved(AutoToggle::Auto, &default),
            completion: resolved(true, &default),
            history: resolved(interactive, &default),
            history_limit: resolved(10_000, &default),
            header: resolved(true, &default),
            null_token: resolved("\\N".to_string(), &default),
        }
    }

    pub fn apply_command_line_overrides(&mut self, final_settings: &Self) {
        overlay_command_line(&mut self.format, &final_settings.format);
        overlay_command_line(&mut self.timing, &final_settings.timing);
        overlay_command_line(&mut self.progress, &final_settings.progress);
        overlay_command_line(&mut self.color, &final_settings.color);
        overlay_command_line(&mut self.rows, &final_settings.rows);
        overlay_command_line(&mut self.width, &final_settings.width);
        overlay_command_line(&mut self.null_display, &final_settings.null_display);
        overlay_command_line(&mut self.multiline, &final_settings.multiline);
        overlay_command_line(&mut self.highlight, &final_settings.highlight);
        overlay_command_line(&mut self.completion, &final_settings.completion);
        overlay_command_line(&mut self.history, &final_settings.history);
        overlay_command_line(&mut self.history_limit, &final_settings.history_limit);
        overlay_command_line(&mut self.header, &final_settings.header);
        overlay_command_line(&mut self.null_token, &final_settings.null_token);
    }
}

fn overlay_command_line<T: Clone>(target: &mut Resolved<T>, source: &Resolved<T>) {
    if source.source == SettingSource::CommandLine {
        *target = source.clone();
    }
}

fn resolved<T>(value: T, source: &SettingSource) -> Resolved<T> {
    Resolved {
        value,
        source: source.clone(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputMode {
    Command(String),
    File(PathBuf),
    Interactive,
    PipedStdin,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutputDestination {
    Stdout { terminal: bool },
    File(PathBuf),
}

#[derive(Debug, Clone)]
pub struct BootstrapPlan {
    pub input: InputMode,
    pub output: OutputDestination,
    pub init: Option<PathBuf>,
    pub startup_settings: Settings,
    pub settings: Settings,
    pub parameters: ParameterStore,
    pub keep_going: bool,
    pub force: bool,
    pub quiet: bool,
    pub stderr_terminal: bool,
    pub config_path: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub enum BootstrapAction {
    Print(String),
    Activate(Box<BootstrapPlan>),
}

pub trait TerminalProbe {
    fn stdin_is_terminal(&self) -> bool;
    fn stdout_is_terminal(&self) -> bool;
    fn stderr_is_terminal(&self) -> bool;
}

pub trait PlatformPaths {
    fn user_config_file(&self) -> Option<PathBuf>;
    fn home_directory(&self) -> Option<PathBuf>;
}

pub trait BootstrapFileSystem {
    fn read(&self, path: &Path) -> std::io::Result<Vec<u8>>;
    fn exists(&self, path: &Path) -> bool;
    fn is_file(&self, path: &Path) -> bool;
    fn parent_is_directory(&self, path: &Path) -> bool;
}

pub trait BootstrapCapabilities: TerminalProbe + PlatformPaths + BootstrapFileSystem {}
impl<T> BootstrapCapabilities for T where T: TerminalProbe + PlatformPaths + BootstrapFileSystem {}

#[derive(Debug, thiserror::Error)]
pub enum BootstrapError {
    #[error("{0}")]
    Usage(String),
    #[error("{path}:{line}: configuration key `{key}` {message}; accepted: {accepted}")]
    Config {
        path: PathBuf,
        line: usize,
        key: String,
        message: String,
        accepted: String,
    },
    #[error("cannot read {kind} `{path}`: {source}")]
    Read {
        kind: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{kind} `{path}` is not valid UTF-8")]
    Utf8 { kind: &'static str, path: PathBuf },
    #[error("{0}")]
    Parameter(#[from] ParameterError),
}

impl BootstrapError {
    pub const fn exit_code(&self) -> u8 {
        2
    }
}

pub fn validate<I, T>(
    arguments: I,
    capabilities: &impl BootstrapCapabilities,
) -> Result<BootstrapAction, BootstrapError>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let matches = match clap_command().try_get_matches_from(arguments) {
        Ok(matches) => matches,
        Err(error) if error.kind() == ErrorKind::DisplayHelp => {
            return Ok(BootstrapAction::Print(error.to_string()));
        }
        Err(error) if error.kind() == ErrorKind::DisplayVersion => {
            return Ok(BootstrapAction::Print(format!(
                "koko {}\nKoko engine {}\n",
                env!("CARGO_PKG_VERSION"),
                version()
            )));
        }
        Err(error) => return Err(BootstrapError::Usage(error.to_string())),
    };

    let input = if let Some(command) = matches.get_one::<String>("command") {
        InputMode::Command(command.clone())
    } else if let Some(path) = matches.get_one::<PathBuf>("file") {
        InputMode::File(validate_input_path(
            path,
            "command file",
            capabilities,
            true,
        )?)
    } else if capabilities.stdin_is_terminal() {
        InputMode::Interactive
    } else {
        InputMode::PipedStdin
    };
    let interactive = input == InputMode::Interactive;
    let mut settings = Settings::defaults(interactive);

    let no_config = matches.get_flag("no-config");
    let mut config_path = None;
    let user_config = (!no_config)
        .then(|| capabilities.user_config_file())
        .flatten()
        .filter(|path| capabilities.exists(path));
    if let Some(path) = user_config {
        let source = read_utf8(&path, "configuration file", capabilities)?;
        let values = parse_config(&path, &source)?;
        apply_config(&mut settings, values, &path);
        config_path = Some(path);
    }

    let startup_settings = settings.clone();
    apply_command_line(&mut settings, &matches)?;

    let init = matches
        .get_one::<PathBuf>("init")
        .map(|path| validate_input_path(path, "initialization file", capabilities, true))
        .transpose()?;

    let mut parameters = ParameterStore::default();
    if let Some(path) = matches.get_one::<PathBuf>("params-file") {
        let path = validate_input_path(path, "parameters file", capabilities, true)?;
        let source = read_utf8(&path, "parameters file", capabilities)?;
        parameters.insert_file_object(&source, path)?;
    }
    if let Some(assignments) = matches.get_many::<String>("param") {
        for assignment in assignments {
            parameters.insert_command_line(assignment)?;
        }
    }

    let force = matches.get_flag("force");
    let output = if let Some(path) = matches.get_one::<PathBuf>("output") {
        let path = validate_local_path(path, capabilities)?;
        if capabilities.exists(&path) && !force {
            return Err(BootstrapError::Usage(format!(
                "output file `{}` already exists; use --force to replace it",
                path.display()
            )));
        }
        if !capabilities.parent_is_directory(&path) {
            return Err(BootstrapError::Usage(format!(
                "output parent for `{}` is not a directory",
                path.display()
            )));
        }
        OutputDestination::File(path)
    } else {
        OutputDestination::Stdout {
            terminal: capabilities.stdout_is_terminal(),
        }
    };

    Ok(BootstrapAction::Activate(Box::new(BootstrapPlan {
        input,
        output,
        init,
        startup_settings,
        settings,
        parameters,
        keep_going: matches.get_flag("keep-going"),
        force,
        quiet: matches.get_flag("quiet"),
        stderr_terminal: capabilities.stderr_is_terminal(),
        config_path,
    })))
}

fn apply_command_line(
    settings: &mut Settings,
    matches: &clap::ArgMatches,
) -> Result<(), BootstrapError> {
    let source = SettingSource::CommandLine;
    if let Some(value) = matches.get_one::<String>("format") {
        settings.format.replace(
            Format::parse(value).expect("clap validates format"),
            source.clone(),
        );
    }
    if matches.get_flag("timing") {
        settings.timing.replace(true, source.clone());
    } else if matches.get_flag("no-timing") {
        settings.timing.replace(false, source.clone());
    }
    if let Some(value) = matches.get_one::<String>("progress") {
        settings.progress.replace(
            AutoToggle::parse(value).expect("clap validates progress"),
            source.clone(),
        );
    }
    if let Some(value) = matches.get_one::<String>("color") {
        settings.color.replace(
            ColorChoice::parse(value).expect("clap validates color"),
            source.clone(),
        );
    }
    if matches.get_flag("header") {
        settings.header.replace(true, source.clone());
    } else if matches.get_flag("no-header") {
        settings.header.replace(false, source.clone());
    }
    if matches.get_flag("no-history") {
        settings.history.replace(false, source.clone());
    }
    if let Some(value) = matches.get_one::<String>("null") {
        settings.null_token.replace(value.clone(), source);
    }
    Ok(())
}

#[derive(Debug, Default)]
struct ConfigValues {
    format: Option<(Format, usize)>,
    timing: Option<(bool, usize)>,
    progress: Option<(AutoToggle, usize)>,
    color: Option<(ColorChoice, usize)>,
    rows: Option<(RowLimit, usize)>,
    width: Option<(WidthLimit, usize)>,
    null_display: Option<(NullDisplay, usize)>,
    multiline: Option<(bool, usize)>,
    highlight: Option<(AutoToggle, usize)>,
    completion: Option<(bool, usize)>,
    history: Option<(bool, usize)>,
    history_limit: Option<(usize, usize)>,
}

fn parse_config(path: &Path, source: &str) -> Result<ConfigValues, BootstrapError> {
    let document = source.parse::<toml_edit::DocumentMut>().map_err(|error| {
        let line = error
            .span()
            .map_or(1, |span| line_number(source, span.start));
        config_error(path, line, "<document>", "is invalid TOML", &["valid TOML"])
    })?;
    let mut values = ConfigValues::default();
    for (key, item) in document.iter() {
        let spec = OPTION_REGISTRY
            .iter()
            .find(|spec| spec.config_key == Some(key))
            .ok_or_else(|| {
                config_error(
                    path,
                    config_item_line(source, key, item),
                    key,
                    "is unknown",
                    &config_keys(),
                )
            })?;
        let line = config_item_line(source, key, item);
        match spec.id {
            OptionId::Format => {
                values.format = Some((
                    config_choice(path, line, key, item, spec, Format::parse)?,
                    line,
                ))
            }
            OptionId::Timing => values.timing = Some((config_bool(path, line, key, item)?, line)),
            OptionId::Progress => {
                values.progress = Some((
                    config_choice(path, line, key, item, spec, AutoToggle::parse)?,
                    line,
                ))
            }
            OptionId::Color => {
                values.color = Some((
                    config_choice(path, line, key, item, spec, ColorChoice::parse)?,
                    line,
                ))
            }
            OptionId::Rows => {
                values.rows = Some((
                    RowLimit::Rows(config_positive(path, line, key, item)?),
                    line,
                ))
            }
            OptionId::Width => {
                let width = if item.as_str() == Some("auto") {
                    WidthLimit::Auto
                } else {
                    WidthLimit::Columns(config_positive(path, line, key, item)?)
                };
                values.width = Some((width, line));
            }
            OptionId::NullDisplay => {
                let value = config_choice(path, line, key, item, spec, |value| match value {
                    "literal" => Some(NullDisplay::Literal),
                    "empty" => Some(NullDisplay::Empty),
                    _ => None,
                })?;
                values.null_display = Some((value, line));
            }
            OptionId::Multiline => {
                values.multiline = Some((config_bool(path, line, key, item)?, line))
            }
            OptionId::Highlight => {
                values.highlight = Some((
                    config_choice(path, line, key, item, spec, AutoToggle::parse)?,
                    line,
                ))
            }
            OptionId::Completion => {
                values.completion = Some((config_bool(path, line, key, item)?, line))
            }
            OptionId::History => values.history = Some((config_bool(path, line, key, item)?, line)),
            OptionId::HistoryLimit => {
                values.history_limit = Some((config_positive(path, line, key, item)?, line))
            }
            _ => {
                return Err(config_error(
                    path,
                    line,
                    key,
                    "is not a configuration setting",
                    spec.accepted,
                ));
            }
        }
    }
    Ok(values)
}

fn apply_config(settings: &mut Settings, values: ConfigValues, path: &Path) {
    macro_rules! apply {
        ($field:ident) => {
            if let Some((value, line)) = values.$field {
                settings.$field.replace(
                    value,
                    SettingSource::Config {
                        path: path.to_path_buf(),
                        line,
                    },
                );
            }
        };
    }
    apply!(format);
    apply!(timing);
    apply!(progress);
    apply!(color);
    apply!(rows);
    apply!(width);
    apply!(null_display);
    apply!(multiline);
    apply!(highlight);
    apply!(completion);
    apply!(history);
    apply!(history_limit);
}

fn config_bool(
    path: &Path,
    line: usize,
    key: &str,
    item: &toml_edit::Item,
) -> Result<bool, BootstrapError> {
    item.as_bool()
        .ok_or_else(|| config_error(path, line, key, "must be a boolean", &["true", "false"]))
}

fn config_positive(
    path: &Path,
    line: usize,
    key: &str,
    item: &toml_edit::Item,
) -> Result<usize, BootstrapError> {
    let value = item
        .as_integer()
        .and_then(|value| usize::try_from(value).ok())
        .filter(|value| *value > 0)
        .ok_or_else(|| config_error(path, line, key, "must be positive", &["positive integer"]))?;
    Ok(value)
}

fn config_choice<T>(
    path: &Path,
    line: usize,
    key: &str,
    item: &toml_edit::Item,
    spec: &OptionSpec,
    parse: impl FnOnce(&str) -> Option<T>,
) -> Result<T, BootstrapError> {
    let value = item
        .as_str()
        .and_then(parse)
        .ok_or_else(|| config_error(path, line, key, "has an invalid value", spec.accepted))?;
    Ok(value)
}

fn config_error(
    path: &Path,
    line: usize,
    key: &str,
    message: &str,
    accepted: &[&str],
) -> BootstrapError {
    BootstrapError::Config {
        path: path.to_path_buf(),
        line,
        key: key.to_string(),
        message: message.to_string(),
        accepted: accepted.join(", "),
    }
}

fn config_item_line(source: &str, key: &str, item: &toml_edit::Item) -> usize {
    item.span().map_or_else(
        || {
            source
                .lines()
                .position(|line| {
                    line.split_once('=')
                        .is_some_and(|(candidate, _)| candidate.trim() == key)
                })
                .map_or(1, |line| line + 1)
        },
        |span| line_number(source, span.start),
    )
}

fn config_keys() -> Vec<&'static str> {
    OPTION_REGISTRY
        .iter()
        .filter_map(|spec| spec.config_key)
        .collect()
}

fn line_number(source: &str, offset: usize) -> usize {
    source[..offset.min(source.len())]
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count()
        + 1
}

fn validate_input_path(
    path: &Path,
    kind: &'static str,
    capabilities: &impl BootstrapCapabilities,
    require_utf8: bool,
) -> Result<PathBuf, BootstrapError> {
    let path = validate_local_path(path, capabilities)?;
    if !capabilities.is_file(&path) {
        return Err(BootstrapError::Usage(format!(
            "{kind} `{}` is not a regular file",
            path.display()
        )));
    }
    if require_utf8 {
        let _ = read_utf8(&path, kind, capabilities)?;
    }
    Ok(path)
}

fn validate_local_path(
    path: &Path,
    capabilities: &impl BootstrapCapabilities,
) -> Result<PathBuf, BootstrapError> {
    if path.to_str().is_some_and(|path| path.contains("://")) {
        return Err(BootstrapError::Usage(format!(
            "remote path `{}` is not supported",
            path.display()
        )));
    }
    expand_tilde(path, capabilities.home_directory()).ok_or_else(|| {
        BootstrapError::Usage(format!(
            "cannot expand `~` in path `{}` without a home directory",
            path.display()
        ))
    })
}

fn expand_tilde(path: &Path, home: Option<PathBuf>) -> Option<PathBuf> {
    let mut components = path.components();
    let Some(Component::Normal(first)) = components.next() else {
        return Some(path.to_path_buf());
    };
    if first != "~" {
        return Some(path.to_path_buf());
    }
    let mut expanded = home?;
    expanded.extend(components);
    Some(expanded)
}

fn read_utf8(
    path: &Path,
    kind: &'static str,
    capabilities: &impl BootstrapFileSystem,
) -> Result<String, BootstrapError> {
    let bytes = capabilities
        .read(path)
        .map_err(|source| BootstrapError::Read {
            kind,
            path: path.to_path_buf(),
            source,
        })?;
    String::from_utf8(bytes).map_err(|_| BootstrapError::Utf8 {
        kind,
        path: path.to_path_buf(),
    })
}
