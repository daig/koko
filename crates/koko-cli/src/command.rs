//! Typed parsing for the sole live meta-command registry.

use crate::registry::{COMMAND_REGISTRY, CommandId, command_spec};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MetaCommand {
    Help(Option<String>),
    Quit {
        rollback: bool,
    },
    Clear,
    Status,
    Graphs,
    Schema(Option<String>),
    Describe(String),
    Functions(Option<String>),
    Parameters {
        values: bool,
    },
    ParameterSet {
        name: String,
        json: String,
    },
    ParameterClear(Option<String>),
    Setting {
        id: CommandId,
        value: Option<String>,
    },
    History(HistoryAction),
    Read {
        path: PathBuf,
        keep_going: bool,
    },
    Output(OutputCommand),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryAction {
    Current,
    Show(Option<usize>),
    Clear,
    On,
    Off,
    Skip,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputMode {
    Refuse,
    Append,
    Replace,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutputCommand {
    Current,
    Stdout,
    File { path: PathBuf, mode: OutputMode },
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CommandError {
    #[error("meta command must start with ':'")]
    MissingPrefix,
    #[error("unknown meta command `:{0}`; use :help to list commands")]
    Unknown(String),
    #[error("unknown meta command `:{name}`; did you mean `:{suggestion}`?")]
    UnknownSuggested { name: String, suggestion: String },
    #[error("command `:{command}` is interactive-only; {guidance}")]
    InteractiveOnly {
        command: String,
        guidance: &'static str,
    },
    #[error("usage: :{0}")]
    Usage(String),
    #[error("unterminated quoted argument")]
    UnterminatedQuote,
}

pub fn parse_meta_command(source: &str, interactive: bool) -> Result<MetaCommand, CommandError> {
    let source = source.trim();
    let body = source
        .strip_prefix(':')
        .ok_or(CommandError::MissingPrefix)?;
    let command_end = body.find(char::is_whitespace).unwrap_or(body.len());
    let name = &body[..command_end];
    let rest = body[command_end..].trim();
    let Some(spec) = command_spec(name) else {
        return Err(nearest_command(name).map_or_else(
            || CommandError::Unknown(name.to_string()),
            |suggestion| CommandError::UnknownSuggested {
                name: name.to_string(),
                suggestion: suggestion.to_string(),
            },
        ));
    };
    if spec.interactive_only && !interactive {
        return Err(CommandError::InteractiveOnly {
            command: spec.name.to_string(),
            guidance: batch_guidance(spec.id),
        });
    }
    parse_registered(spec.id, rest).map_err(|error| match error {
        CommandError::Usage(_) => CommandError::Usage(
            format!("{} {}", spec.name, spec.arguments)
                .trim()
                .to_string(),
        ),
        other => other,
    })
}

fn parse_registered(id: CommandId, rest: &str) -> Result<MetaCommand, CommandError> {
    match id {
        CommandId::Help => Ok(MetaCommand::Help(optional_one(rest)?)),
        CommandId::Quit => match words(rest)?.as_slice() {
            [] => Ok(MetaCommand::Quit { rollback: false }),
            [flag] if flag == "--rollback" => Ok(MetaCommand::Quit { rollback: true }),
            _ => usage(),
        },
        CommandId::Clear => no_arguments(rest, MetaCommand::Clear),
        CommandId::Status => no_arguments(rest, MetaCommand::Status),
        CommandId::Graphs => no_arguments(rest, MetaCommand::Graphs),
        CommandId::Schema => Ok(MetaCommand::Schema(optional_one(rest)?)),
        CommandId::Describe => Ok(MetaCommand::Describe(required_one(rest)?)),
        CommandId::Functions => Ok(MetaCommand::Functions(optional_one(rest)?)),
        CommandId::Parameters => match words(rest)?.as_slice() {
            [] => Ok(MetaCommand::Parameters { values: false }),
            [flag] if flag == "--values" => Ok(MetaCommand::Parameters { values: true }),
            _ => usage(),
        },
        CommandId::Parameter => parse_parameter(rest),
        CommandId::Format
        | CommandId::Timing
        | CommandId::Progress
        | CommandId::Rows
        | CommandId::Width
        | CommandId::Null
        | CommandId::Multiline
        | CommandId::Highlight
        | CommandId::Completion => Ok(MetaCommand::Setting {
            id,
            value: optional_one(rest)?,
        }),
        CommandId::History => parse_history(rest),
        CommandId::Read => parse_read(rest),
        CommandId::Output => parse_output(rest),
    }
}

fn parse_parameter(rest: &str) -> Result<MetaCommand, CommandError> {
    let first_end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    let name = &rest[..first_end];
    let value = rest[first_end..].trim();
    if name.eq_ignore_ascii_case("clear") {
        return match words(value)?.as_slice() {
            [target] if target.eq_ignore_ascii_case("all") => Ok(MetaCommand::ParameterClear(None)),
            [target] => Ok(MetaCommand::ParameterClear(Some(target.clone()))),
            _ => usage(),
        };
    }
    if name.is_empty() || value.is_empty() {
        return usage();
    }
    Ok(MetaCommand::ParameterSet {
        name: name.to_string(),
        json: value.to_string(),
    })
}

fn parse_history(rest: &str) -> Result<MetaCommand, CommandError> {
    let parsed = match words(rest)?.as_slice() {
        [] => HistoryAction::Current,
        [action] if action.eq_ignore_ascii_case("show") => HistoryAction::Show(None),
        [action, count] if action.eq_ignore_ascii_case("show") => HistoryAction::Show(Some(
            count
                .parse()
                .map_err(|_| CommandError::Usage(String::new()))?,
        )),
        [action] if action.eq_ignore_ascii_case("clear") => HistoryAction::Clear,
        [action] if action.eq_ignore_ascii_case("on") => HistoryAction::On,
        [action] if action.eq_ignore_ascii_case("off") => HistoryAction::Off,
        [action] if action.eq_ignore_ascii_case("skip") => HistoryAction::Skip,
        _ => return usage(),
    };
    Ok(MetaCommand::History(parsed))
}

fn parse_read(rest: &str) -> Result<MetaCommand, CommandError> {
    match words(rest)?.as_slice() {
        [path] => Ok(MetaCommand::Read {
            path: path.into(),
            keep_going: false,
        }),
        [path, flag] if flag == "--keep-going" => Ok(MetaCommand::Read {
            path: path.into(),
            keep_going: true,
        }),
        _ => usage(),
    }
}

fn parse_output(rest: &str) -> Result<MetaCommand, CommandError> {
    match words(rest)?.as_slice() {
        [] => Ok(MetaCommand::Output(OutputCommand::Current)),
        [target] if target.eq_ignore_ascii_case("stdout") => {
            Ok(MetaCommand::Output(OutputCommand::Stdout))
        }
        [path] => Ok(MetaCommand::Output(OutputCommand::File {
            path: path.into(),
            mode: OutputMode::Refuse,
        })),
        [path, mode] => {
            let mode = if mode.eq_ignore_ascii_case("append") {
                OutputMode::Append
            } else if mode.eq_ignore_ascii_case("replace") {
                OutputMode::Replace
            } else {
                return usage();
            };
            Ok(MetaCommand::Output(OutputCommand::File {
                path: path.into(),
                mode,
            }))
        }
        _ => usage(),
    }
}

fn optional_one(rest: &str) -> Result<Option<String>, CommandError> {
    match words(rest)?.as_slice() {
        [] => Ok(None),
        [value] => Ok(Some(value.clone())),
        _ => usage(),
    }
}

fn required_one(rest: &str) -> Result<String, CommandError> {
    optional_one(rest)?.ok_or_else(|| CommandError::Usage(String::new()))
}

fn no_arguments(rest: &str, command: MetaCommand) -> Result<MetaCommand, CommandError> {
    if rest.is_empty() {
        Ok(command)
    } else {
        usage()
    }
}

fn usage<T>() -> Result<T, CommandError> {
    Err(CommandError::Usage(String::new()))
}

fn nearest_command(name: &str) -> Option<&'static str> {
    COMMAND_REGISTRY
        .iter()
        .map(|spec| (strsim::levenshtein(name, spec.name), spec.name))
        .min_by_key(|(distance, _)| *distance)
        .filter(|(distance, _)| *distance <= 3)
        .map(|(_, command)| command)
}

fn batch_guidance(id: CommandId) -> &'static str {
    match id {
        CommandId::Help => "use koko --help",
        CommandId::Clear => "terminal clearing is unavailable in batch input",
        CommandId::Rows => "batch output is never display-truncated",
        CommandId::Multiline => "batch statement boundaries are parser-defined",
        CommandId::Highlight => "batch output is never syntax-highlighted",
        CommandId::Completion => "completion is available only in the editor",
        CommandId::History => "history is available only in the editor",
        _ => "run this command in an interactive session",
    }
}

fn words(source: &str) -> Result<Vec<String>, CommandError> {
    let mut values = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut escaped = false;
    let mut started = false;
    for character in source.chars() {
        if escaped {
            current.push(character);
            escaped = false;
            started = true;
            continue;
        }
        match quote {
            Some(mark) if character == mark => quote = None,
            Some('\'') => current.push(character),
            Some(_) if character == '\\' => escaped = true,
            Some(_) => current.push(character),
            None if character == '\'' || character == '"' => {
                quote = Some(character);
                started = true;
            }
            None if character == '\\' => {
                escaped = true;
                started = true;
            }
            None if character.is_whitespace() => {
                if started {
                    values.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            None => {
                current.push(character);
                started = true;
            }
        }
    }
    if quote.is_some() || escaped {
        return Err(CommandError::UnterminatedQuote);
    }
    if started {
        values.push(current);
    }
    Ok(values)
}
