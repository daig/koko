//! Reedline adapters for the interactive prompt, canonical validation, and metadata completion.

use crate::app_output::OutputManager;
use crate::bootstrap::{AutoToggle, ColorChoice, Settings};
use crate::command::{HistoryAction, MetaCommand, parse_meta_command};
use crate::continuation::normalize_continuations;
use crate::history::{HistoryController, KokoHistory};
use crate::registry::{COMMAND_REGISTRY, CommandId, CompletionKind};
use crate::runner::{RunSummary, RunnerError, SessionState, SourceRunner};
use koko::tooling::{
    CatalogSnapshot, CursorContextKind, FunctionKind, GraphKind, SyntaxStatus, TokenKind,
    TransactionMode, analyze_cypher, cypher_keywords, version,
};
use nu_ansi_term::{Color, Style};
use reedline::{
    Completer, EditCommand, Emacs, Highlighter, History, IdeMenu, KeyCode, KeyModifiers,
    Keybindings, MenuBuilder, Prompt, PromptEditMode, PromptHistorySearch,
    PromptHistorySearchStatus, Reedline, ReedlineEvent, ReedlineMenu, Signal, Span, StyledText,
    Suggestion, ValidationResult, Validator, default_emacs_keybindings,
};
use std::borrow::Cow;
use std::collections::BTreeSet;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::{Duration, Instant};

const COMPLETION_MENU: &str = "koko_completion";
const INPUT_INTERRUPT: &str = "koko:input-interrupt";
const COMPLETION_LIMIT: usize = 256;

#[derive(Debug, Clone)]
struct Candidate {
    value: String,
    kind: &'static str,
    detail: Option<String>,
    priority: u8,
}

impl Candidate {
    fn new(
        value: impl Into<String>,
        kind: &'static str,
        detail: Option<String>,
        priority: u8,
    ) -> Self {
        Self {
            value: value.into(),
            kind,
            detail,
            priority,
        }
    }

    fn description(&self) -> String {
        self.detail.as_ref().map_or_else(
            || self.kind.to_string(),
            |detail| format!("{}  {detail}", self.kind),
        )
    }
}

#[derive(Debug, Clone, Default)]
struct CompletionCatalog {
    graphs: Vec<Candidate>,
    node_labels: Vec<Candidate>,
    relationship_labels: Vec<Candidate>,
    properties: Vec<Candidate>,
    functions: Vec<Candidate>,
    settings: Vec<Candidate>,
}

impl CompletionCatalog {
    fn from_snapshot(snapshot: &CatalogSnapshot) -> Self {
        let graphs = snapshot
            .graphs()
            .iter()
            .map(|graph| {
                Candidate::new(
                    graph.name(),
                    "graph",
                    Some(match graph.kind() {
                        GraphKind::Typed => "typed".to_string(),
                        GraphKind::Any => "ANY".to_string(),
                        _ => "unknown".to_string(),
                    }),
                    1,
                )
            })
            .collect();
        let node_labels = snapshot
            .node_tables()
            .iter()
            .map(|table| Candidate::new(table.name(), "node label", None, 1))
            .collect();
        let relationship_labels = snapshot
            .relationship_tables()
            .iter()
            .map(|table| Candidate::new(table.name(), "relationship label", None, 1))
            .collect();
        let mut properties = Vec::new();
        for table in snapshot.node_tables() {
            for property in table.properties() {
                properties.push(Candidate::new(
                    property.name(),
                    "property",
                    Some(format!("{}  {}", property.type_text(), table.name())),
                    1,
                ));
            }
        }
        for table in snapshot.relationship_tables() {
            for property in table.properties() {
                properties.push(Candidate::new(
                    property.name(),
                    "property",
                    Some(format!("{}  {}", property.type_text(), table.name())),
                    1,
                ));
            }
        }
        let functions = snapshot
            .functions()
            .iter()
            .map(|function| {
                Candidate::new(
                    function.name(),
                    match function.kind() {
                        FunctionKind::Scalar | FunctionKind::ConnectionLocal => "scalar function",
                        FunctionKind::Aggregate => "aggregate",
                        FunctionKind::Table => "table function",
                        FunctionKind::Macro => "scalar function",
                        _ => "function",
                    },
                    Some(format!(
                        "{} -> {}",
                        function.signature(),
                        function.return_type()
                    )),
                    2,
                )
            })
            .collect();
        let settings = snapshot
            .settings()
            .iter()
            .map(|setting| {
                let accepted = setting.accepted_values().join("/");
                Candidate::new(
                    setting.name(),
                    "setting",
                    Some(if accepted.is_empty() {
                        setting.logical_type().to_string()
                    } else {
                        accepted
                    }),
                    1,
                )
            })
            .collect();
        Self {
            graphs,
            node_labels,
            relationship_labels,
            properties,
            functions,
            settings,
        }
    }
}

#[derive(Debug, Clone)]
struct DynamicEditorState {
    completion: bool,
    highlighting: bool,
    multiline: bool,
    parameters: Vec<String>,
    catalog: CompletionCatalog,
}

impl DynamicEditorState {
    fn new(settings: &Settings, color_allowed: bool) -> Self {
        Self {
            completion: *settings.completion.value(),
            highlighting: highlight_allowed(settings, color_allowed),
            multiline: *settings.multiline.value(),
            parameters: Vec::new(),
            catalog: CompletionCatalog::default(),
        }
    }
}

#[derive(Debug, Clone)]
struct SharedEditorState(Arc<RwLock<DynamicEditorState>>);

impl SharedEditorState {
    fn read(&self) -> RwLockReadGuard<'_, DynamicEditorState> {
        self.0
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn write(&self) -> RwLockWriteGuard<'_, DynamicEditorState> {
        self.0
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

pub fn run_interactive(
    state: &mut SessionState,
    output: &mut OutputManager,
    quiet: bool,
    history_path: Option<PathBuf>,
) -> Result<RunSummary, RunnerError> {
    let hard_disabled = !*state.settings.history.value()
        && matches!(
            state.settings.history.source(),
            crate::bootstrap::SettingSource::CommandLine
        );
    let history_limit = *state.settings.history_limit.value();
    let (history_control, history) = HistoryController::open(
        history_path,
        history_limit,
        *state.settings.history.value(),
        hard_disabled,
    )?;
    let color_allowed = color_allowed(&state.settings);
    let shared = SharedEditorState(Arc::new(RwLock::new(DynamicEditorState::new(
        &state.settings,
        color_allowed,
    ))));
    let dumb = std::env::var("TERM").is_ok_and(|term| term.eq_ignore_ascii_case("dumb"))
        || !std::io::stderr().is_terminal()
        || !std::io::stdout().is_terminal();
    let mut runner =
        SourceRunner::new(state, output, true, true).with_history(history_control.clone());
    if !quiet {
        let graph = runner
            .session_snapshot()
            .map(|session| session.graph().name().to_string())
            .unwrap_or_else(|_| "?".to_string());
        runner.diagnostic(&format!(
            "Koko {} · in-memory · graph {graph}\nType :help for help; Ctrl-D or :quit to exit.\n",
            version()
        ))?;
    }
    refresh_editor_state(&runner, &shared, color_allowed);
    let result = if dumb {
        run_dumb_loop(&mut runner, &history_control, history)
    } else {
        run_reedline_loop(
            &mut runner,
            &history_control,
            history,
            shared,
            color_allowed,
        )
    };
    let restoration = restore_terminal(!dumb);
    if let Err(error) = history_control.sync() {
        runner.diagnostic(&format!("History could not be saved: {error}"))?;
    }
    match result {
        Err(error) => Err(error),
        Ok(summary) => {
            restoration?;
            Ok(summary)
        }
    }
}

fn restore_terminal(editor_used: bool) -> io::Result<()> {
    if !editor_used {
        return Ok(());
    }
    crossterm::terminal::disable_raw_mode()?;
    crossterm::execute!(
        std::io::stderr(),
        crossterm::event::DisableBracketedPaste,
        crossterm::event::DisableMouseCapture,
        crossterm::cursor::Show
    )
}

fn koko_keybindings() -> Keybindings {
    let mut keybindings = default_emacs_keybindings();
    keybindings.add_binding(
        KeyModifiers::NONE,
        KeyCode::Tab,
        ReedlineEvent::UntilFound(vec![
            ReedlineEvent::Menu(COMPLETION_MENU.to_string()),
            ReedlineEvent::MenuNext,
        ]),
    );
    keybindings.add_binding(
        KeyModifiers::SHIFT,
        KeyCode::BackTab,
        ReedlineEvent::UntilFound(vec![
            ReedlineEvent::MenuPrevious,
            ReedlineEvent::Menu(COMPLETION_MENU.to_string()),
        ]),
    );
    keybindings.add_binding(
        KeyModifiers::CONTROL,
        KeyCode::Char('g'),
        ReedlineEvent::Multiple(vec![
            ReedlineEvent::Esc,
            ReedlineEvent::Edit(vec![EditCommand::Clear]),
        ]),
    );
    keybindings.add_binding(
        KeyModifiers::CONTROL,
        KeyCode::Char('c'),
        ReedlineEvent::ExecuteHostCommand(INPUT_INTERRUPT.to_string()),
    );
    keybindings.add_binding(
        KeyModifiers::CONTROL,
        KeyCode::Char('j'),
        ReedlineEvent::Submit,
    );
    keybindings.add_binding(
        KeyModifiers::CONTROL,
        KeyCode::Char('s'),
        ReedlineEvent::NextHistory,
    );
    keybindings
}

fn run_reedline_loop(
    runner: &mut SourceRunner<'_>,
    history_control: &HistoryController,
    history: KokoHistory,
    shared: SharedEditorState,
    color_allowed: bool,
) -> Result<RunSummary, RunnerError> {
    let keybindings = koko_keybindings();
    let menu = Box::new(IdeMenu::default().with_name(COMPLETION_MENU));
    let mut editor = Reedline::create()
        .with_history(Box::new(history))
        .with_validator(Box::new(KokoValidator(shared.clone())))
        .with_highlighter(Box::new(KokoHighlighter(shared.clone())))
        .with_completer(Box::new(KokoCompleter(shared.clone())))
        .with_menu(ReedlineMenu::EngineCompleter(menu))
        .with_edit_mode(Box::new(Emacs::new(keybindings)))
        .with_quick_completions(true)
        .with_ansi_colors(color_allowed);
    let mut last_empty_interrupt = None;
    loop {
        let prompt = prompt_for(runner);
        let signal = editor
            .read_line(&prompt)
            .map_err(|error| RunnerError::Editor(error.to_string()))?;
        match signal {
            Signal::Success(input) => {
                last_empty_interrupt = None;
                let (input, _) = normalize_continuations(&input);
                if input.trim().is_empty() {
                    continue;
                }
                if is_history_clear(&input) && !confirm_history_clear(&mut editor, history_control)?
                {
                    runner.diagnostic("History unchanged.")?;
                    continue;
                }
                if is_clear_command(&input) {
                    clear_terminal()?;
                    continue;
                }
                let input = normalize_pasted_tabs(&input);
                let summary = match runner.run_interactive_input(&input) {
                    Ok(summary) => summary,
                    Err(error) if error.is_interactive_recoverable() => {
                        runner.diagnostic(&error.to_string())?;
                        refresh_editor_state(runner, &shared, color_allowed);
                        continue;
                    }
                    Err(error) => return Err(error),
                };
                if summary.quit {
                    return Ok(RunSummary {
                        failed: false,
                        quit: true,
                        interrupted: false,
                    });
                }
                refresh_editor_state(runner, &shared, color_allowed);
            }
            Signal::CtrlD => {
                if runner.request_exit()? {
                    return Ok(RunSummary {
                        failed: false,
                        quit: true,
                        interrupted: false,
                    });
                }
            }
            Signal::CtrlC => {
                if handle_empty_interrupt(runner, &mut last_empty_interrupt)? {
                    return Ok(RunSummary {
                        failed: false,
                        quit: true,
                        interrupted: true,
                    });
                }
            }
            Signal::ExternalBreak(_) => {
                runner.diagnostic("Input cancelled.")?;
            }
            Signal::HostCommand(command) if command == INPUT_INTERRUPT => {
                let was_empty = editor.current_buffer_contents().is_empty();
                editor.run_edit_commands(&[EditCommand::Clear]);
                if was_empty {
                    if handle_empty_interrupt(runner, &mut last_empty_interrupt)? {
                        return Ok(RunSummary {
                            failed: false,
                            quit: true,
                            interrupted: true,
                        });
                    }
                } else {
                    last_empty_interrupt = None;
                }
            }
            Signal::HostCommand(_) => {}
            _ => {}
        }
    }
}

fn handle_empty_interrupt(
    runner: &mut SourceRunner<'_>,
    previous: &mut Option<Instant>,
) -> Result<bool, RunnerError> {
    let now = Instant::now();
    if previous.is_some_and(|last| now.duration_since(last) <= Duration::from_secs(2))
        && runner
            .session_snapshot()
            .is_ok_and(|session| session.transaction() == TransactionMode::None)
    {
        return Ok(true);
    }
    runner.diagnostic("Press Ctrl-D or :quit to exit")?;
    *previous = Some(now);
    Ok(false)
}

fn run_dumb_loop(
    runner: &mut SourceRunner<'_>,
    history_control: &HistoryController,
    mut history: KokoHistory,
) -> Result<RunSummary, RunnerError> {
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let mut pending = String::new();
    loop {
        let prompt = if pending.is_empty() {
            prompt_for(runner).text()
        } else {
            "...> ".to_string()
        };
        let mut stderr = std::io::stderr().lock();
        stderr.write_all(prompt.as_bytes())?;
        stderr.flush()?;
        drop(stderr);
        let mut line = String::new();
        if input
            .read_line(&mut line)
            .map_err(|source| RunnerError::Read {
                path: PathBuf::from("<stdin>"),
                source,
            })?
            == 0
        {
            if runner.request_exit()? {
                return Ok(RunSummary {
                    failed: false,
                    quit: true,
                    interrupted: false,
                });
            }
            continue;
        }
        pending.push_str(&line);
        let requested_more = {
            let (normalized, requested_more) = normalize_continuations(&pending);
            if let Cow::Owned(normalized) = normalized {
                pending = normalized;
            }
            requested_more
        };
        if requested_more {
            continue;
        }
        let command = pending.trim_start().starts_with(':');
        if !command
            && runner.settings().multiline.value() == &true
            && analyze_cypher(&pending, None).status() == SyntaxStatus::Incomplete
        {
            continue;
        }
        history
            .save(reedline::HistoryItem::from_command_line(&pending))
            .map_err(|error| RunnerError::Editor(error.to_string()))?;
        if is_history_clear(&pending) {
            runner.diagnostic(
                "History was not cleared; confirmation menus require a capable terminal.",
            )?;
            pending.clear();
            continue;
        }
        let statement = normalize_pasted_tabs(&pending);
        let result = runner.run_interactive_input(&statement);
        pending.clear();
        let summary = match result {
            Ok(summary) => summary,
            Err(error) if error.is_interactive_recoverable() => {
                runner.diagnostic(&error.to_string())?;
                continue;
            }
            Err(error) => return Err(error),
        };
        if summary.quit {
            history_control.sync()?;
            return Ok(RunSummary {
                failed: false,
                quit: true,
                interrupted: false,
            });
        }
    }
}

fn refresh_editor_state(
    runner: &SourceRunner<'_>,
    shared: &SharedEditorState,
    color_allowed: bool,
) {
    let mut state = shared.write();
    state.completion = *runner.settings().completion.value();
    state.highlighting = highlight_allowed(runner.settings(), color_allowed);
    state.multiline = *runner.settings().multiline.value();
    state.parameters = runner
        .parameters()
        .entries()
        .map(|entry| entry.name().to_string())
        .collect();
    if let Ok(catalog) = runner.catalog_snapshot() {
        state.catalog = CompletionCatalog::from_snapshot(&catalog);
    } else {
        state.catalog = CompletionCatalog::default();
    }
}

fn color_allowed(settings: &Settings) -> bool {
    match settings.color.value() {
        ColorChoice::Always => true,
        ColorChoice::Never => false,
        ColorChoice::Auto => {
            std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none()
        }
    }
}

fn highlight_allowed(settings: &Settings, color_allowed: bool) -> bool {
    color_allowed && !matches!(settings.highlight.value(), AutoToggle::Off)
}

#[derive(Debug, Clone)]
struct KokoValidator(SharedEditorState);

impl Validator for KokoValidator {
    fn validate(&self, line: &str) -> ValidationResult {
        let (normalized, requested_more) = normalize_continuations(line);
        if requested_more {
            return ValidationResult::Incomplete;
        }
        if !self.0.read().multiline || line.trim_start().starts_with(':') {
            return ValidationResult::Complete;
        }
        if analyze_cypher(&normalized, None).status() == SyntaxStatus::Incomplete {
            ValidationResult::Incomplete
        } else {
            ValidationResult::Complete
        }
    }
}

#[derive(Debug, Clone)]
struct KokoHighlighter(SharedEditorState);

impl Highlighter for KokoHighlighter {
    fn highlight(&self, line: &str, _cursor: usize) -> StyledText {
        let mut styled = StyledText::new();
        styled.push((Style::new(), line.to_string()));
        if !self.0.read().highlighting || line.trim_start().starts_with(':') {
            return styled;
        }
        let analysis = analyze_cypher(line, None);
        for token in analysis.tokens() {
            let style = match token.kind() {
                TokenKind::Keyword => Style::new().fg(Color::Blue).bold(),
                TokenKind::Identifier => Style::new().fg(Color::Cyan),
                TokenKind::Parameter => Style::new().fg(Color::Purple),
                TokenKind::String => Style::new().fg(Color::Green),
                TokenKind::Number => Style::new().fg(Color::Yellow),
                TokenKind::Comment => Style::new().fg(Color::DarkGray).italic(),
                TokenKind::Punctuation | TokenKind::Operator => Style::new().fg(Color::LightGray),
                _ => Style::new(),
            };
            let span = token.span();
            styled.style_range(span.start(), span.end(), style);
        }
        if let Some(span) = analysis
            .diagnostic()
            .and_then(|diagnostic| diagnostic.span())
        {
            styled.style_range(
                span.start(),
                span.end(),
                Style::new().underline().fg(Color::Red),
            );
        }
        styled
    }
}

#[derive(Debug, Clone)]
struct KokoCompleter(SharedEditorState);

impl Completer for KokoCompleter {
    fn complete(&mut self, line: &str, pos: usize) -> Vec<Suggestion> {
        let state = self.0.read();
        if !state.completion || pos > line.len() || !line.is_char_boundary(pos) {
            return Vec::new();
        }
        if line[..pos].trim_start().starts_with(':') {
            return complete_meta(line, pos, &state);
        }
        complete_cypher(line, pos, &state)
    }
}

fn complete_meta(line: &str, pos: usize, state: &DynamicEditorState) -> Vec<Suggestion> {
    let before = &line[..pos];
    let leading = before.len() - before.trim_start().len();
    let logical = &before[leading..];
    let command_end = logical.find(char::is_whitespace);
    if command_end.is_none() {
        let prefix = logical.strip_prefix(':').unwrap_or(logical);
        let candidates = COMMAND_REGISTRY
            .iter()
            .map(|command| {
                Candidate::new(
                    format!(":{}", command.name),
                    "command",
                    Some(format!("{}  {}", command.arguments, command.summary)),
                    0,
                )
            })
            .collect();
        return suggestions(candidates, prefix, Span::new(leading, pos), Some(':'));
    }
    let command_end = command_end.expect("checked command boundary");
    let command_name = &logical[1..command_end];
    let Some(spec) = COMMAND_REGISTRY
        .iter()
        .find(|spec| spec.name.eq_ignore_ascii_case(command_name))
    else {
        return Vec::new();
    };
    let argument_start = leading + command_end + 1;
    let argument = before[argument_start.min(pos)..].trim_start();
    let replacement_start = pos.saturating_sub(
        argument
            .rsplit_once(char::is_whitespace)
            .map_or(argument.len(), |(_, tail)| tail.len()),
    );
    let prefix = &line[replacement_start..pos];
    let candidates = match spec.completion {
        CompletionKind::None => setting_values(spec.id),
        CompletionKind::HelpTopic => COMMAND_REGISTRY
            .iter()
            .map(|command| {
                Candidate::new(
                    command.name,
                    "command",
                    Some(command.summary.to_string()),
                    0,
                )
            })
            .collect(),
        CompletionKind::GraphObject => state
            .catalog
            .graphs
            .iter()
            .chain(state.catalog.node_labels.iter())
            .chain(state.catalog.relationship_labels.iter())
            .cloned()
            .collect(),
        CompletionKind::Function => state.catalog.functions.clone(),
        CompletionKind::Parameter => state
            .parameters
            .iter()
            .map(|name| Candidate::new(name, "parameter", None, 0))
            .collect(),
        CompletionKind::Setting => setting_values(spec.id),
        CompletionKind::Path | CompletionKind::OutputPath => complete_path_candidates(prefix),
    };
    suggestions(candidates, prefix, Span::new(replacement_start, pos), None)
}

fn setting_values(id: CommandId) -> Vec<Candidate> {
    let values: &[&str] = match id {
        CommandId::Format => &[
            "auto", "box", "table", "csv", "tsv", "json", "jsonl", "markdown", "line", "trash",
        ],
        CommandId::Timing | CommandId::Multiline | CommandId::Completion => &["on", "off"],
        CommandId::Progress | CommandId::Highlight => &["auto", "on", "off"],
        CommandId::Rows => &["all", "default"],
        CommandId::Width => &["auto"],
        CommandId::Null => &["literal", "empty"],
        CommandId::History => &["show", "clear", "on", "off", "skip"],
        CommandId::Output => &["stdout", "append", "replace"],
        _ => &[],
    };
    values
        .iter()
        .map(|value| Candidate::new(*value, "setting", None, 0))
        .collect()
}

fn complete_cypher(line: &str, pos: usize, state: &DynamicEditorState) -> Vec<Suggestion> {
    let analysis = analyze_cypher(line, Some(pos));
    let in_string_or_comment = analysis.tokens().iter().any(|token| {
        let span = token.span();
        span.start() < pos
            && pos <= span.end()
            && matches!(token.kind(), TokenKind::Comment | TokenKind::String)
    });
    let completing_path = analysis
        .cursor_context()
        .is_some_and(|context| context.kind() == CursorContextKind::Path);
    if in_string_or_comment && !completing_path {
        return Vec::new();
    }
    let Some(context) = analysis.cursor_context() else {
        return Vec::new();
    };
    if context.kind() == CursorContextKind::Path {
        let start = path_prefix_start(line, pos);
        let prefix = &line[start..pos];
        return suggestions(
            complete_path_candidates(prefix),
            prefix,
            Span::new(start, pos),
            None,
        );
    }
    let mut candidates = match context.kind() {
        CursorContextKind::Keyword => cypher_keywords()
            .iter()
            .map(|keyword| Candidate::new(*keyword, "keyword", None, 5))
            .collect(),
        CursorContextKind::Graph => state.catalog.graphs.clone(),
        CursorContextKind::NodeLabel => state.catalog.node_labels.clone(),
        CursorContextKind::RelationshipLabel => state.catalog.relationship_labels.clone(),
        CursorContextKind::Property => state.catalog.properties.clone(),
        CursorContextKind::Function => state.catalog.functions.clone(),
        CursorContextKind::Parameter => state
            .parameters
            .iter()
            .map(|name| Candidate::new(format!("${name}"), "parameter", None, 0))
            .collect(),
        CursorContextKind::Setting => state.catalog.settings.clone(),
        CursorContextKind::Variable => variables_in_scope(line, pos),
        CursorContextKind::Path => unreachable!("path handled above"),
        _ => Vec::new(),
    };
    if context.kind() != CursorContextKind::Keyword {
        candidates.extend(
            cypher_keywords()
                .iter()
                .map(|keyword| Candidate::new(*keyword, "keyword", None, 6)),
        );
    }
    let span = context.replacement();
    suggestions(
        candidates,
        context.prefix(),
        Span::new(span.start(), span.end()),
        None,
    )
}

fn variables_in_scope(line: &str, pos: usize) -> Vec<Candidate> {
    let mut seen = BTreeSet::new();
    analyze_cypher(&line[..pos], None)
        .tokens()
        .iter()
        .filter(|token| token.kind() == TokenKind::Identifier)
        .filter_map(|token| {
            let span = token.span();
            let value = &line[span.start()..span.end()];
            seen.insert(value.to_ascii_lowercase())
                .then(|| Candidate::new(value, "variable", None, 0))
        })
        .collect()
}

fn suggestions(
    candidates: Vec<Candidate>,
    prefix: &str,
    span: Span,
    command_prefix: Option<char>,
) -> Vec<Suggestion> {
    let prefix = prefix
        .strip_prefix(command_prefix.unwrap_or('\0'))
        .unwrap_or(prefix);
    let folded = prefix.to_lowercase();
    let mut candidates = candidates
        .into_iter()
        .filter_map(|candidate| {
            let value = candidate
                .value
                .strip_prefix(command_prefix.unwrap_or('\0'))
                .unwrap_or(&candidate.value);
            let value_folded = value.to_lowercase();
            let rank = if value_folded.starts_with(&folded) {
                0
            } else if value_folded.contains(&folded) {
                1
            } else {
                return None;
            };
            Some((rank, candidate.priority, candidate))
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| {
        (left.0, left.1, left.2.value.to_ascii_lowercase()).cmp(&(
            right.0,
            right.1,
            right.2.value.to_ascii_lowercase(),
        ))
    });
    let mut seen = BTreeSet::new();
    candidates
        .into_iter()
        .filter(|(_, _, candidate)| seen.insert(candidate.value.to_ascii_lowercase()))
        .take(COMPLETION_LIMIT)
        .map(|(_, _, candidate)| {
            let description = candidate.description();
            Suggestion {
                value: candidate.value,
                description: Some(description),
                span,
                append_whitespace: candidate.kind != "path",
                ..Suggestion::default()
            }
        })
        .collect()
}

fn complete_path_candidates(prefix: &str) -> Vec<Candidate> {
    let quote = prefix
        .chars()
        .next()
        .filter(|character| *character == '\'' || *character == '"');
    let raw = quote.map_or(prefix, |quote| prefix.strip_prefix(quote).unwrap_or(prefix));
    let expanded = if let Some(rest) = raw.strip_prefix("~/") {
        directories::BaseDirs::new()
            .map(|base| base.home_dir().join(rest))
            .unwrap_or_else(|| PathBuf::from(raw))
    } else {
        PathBuf::from(raw)
    };
    let (directory, file_prefix) = if raw.ends_with(std::path::MAIN_SEPARATOR) {
        (expanded.as_path(), "")
    } else {
        (
            expanded.parent().unwrap_or_else(|| Path::new(".")),
            expanded
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(""),
        )
    };
    let mut entries = std::fs::read_dir(directory)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            if !name.to_lowercase().starts_with(&file_prefix.to_lowercase()) {
                return None;
            }
            let mut value = if raw.ends_with(std::path::MAIN_SEPARATOR) {
                format!("{raw}{name}")
            } else if let Some(parent) = Path::new(raw)
                .parent()
                .filter(|path| !path.as_os_str().is_empty())
            {
                parent.join(&name).to_string_lossy().into_owned()
            } else {
                name
            };
            if entry.file_type().ok()?.is_dir() {
                value.push(std::path::MAIN_SEPARATOR);
            }
            if let Some(quote) = quote {
                value.insert(0, quote);
            }
            Some(Candidate::new(value, "path", None, 0))
        })
        .take(COMPLETION_LIMIT)
        .collect::<Vec<_>>();
    entries.sort_by(|left, right| left.value.cmp(&right.value));
    entries
}

fn path_prefix_start(line: &str, pos: usize) -> usize {
    let before = &line[..pos];
    before
        .char_indices()
        .rev()
        .find(|(_, character)| character.is_whitespace() || matches!(character, '\'' | '"'))
        .map_or(0, |(index, character)| {
            index + usize::from(character.is_whitespace())
        })
}

fn normalize_pasted_tabs(input: &str) -> String {
    if !input.contains('\t') {
        return input.to_string();
    }
    let string_spans = analyze_cypher(input, None)
        .tokens()
        .iter()
        .filter(|token| token.kind() == TokenKind::String)
        .map(|token| token.span())
        .collect::<Vec<_>>();
    let mut output = String::with_capacity(input.len());
    for (index, character) in input.char_indices() {
        if character == '\t'
            && !string_spans
                .iter()
                .any(|span| span.start() <= index && index < span.end())
        {
            output.push_str("    ");
        } else {
            output.push(character);
        }
    }
    output
}

fn is_history_clear(input: &str) -> bool {
    matches!(
        parse_meta_command(input.trim(), true),
        Ok(MetaCommand::History(HistoryAction::Clear))
    )
}

fn is_clear_command(input: &str) -> bool {
    matches!(
        parse_meta_command(input.trim(), true),
        Ok(MetaCommand::Clear)
    )
}

fn confirm_history_clear(
    editor: &mut Reedline,
    history: &HistoryController,
) -> Result<bool, RunnerError> {
    history.suppress_next();
    let signal = editor
        .read_line(&ConfirmationPrompt)
        .map_err(|error| RunnerError::Editor(error.to_string()))?;
    let confirmed = matches!(signal, Signal::Success(answer) if answer.trim().eq_ignore_ascii_case("y") || answer.trim().eq_ignore_ascii_case("yes"));
    if confirmed {
        history.confirm_clear();
    }
    Ok(confirmed)
}

fn clear_terminal() -> Result<(), RunnerError> {
    use crossterm::{ExecutableCommand, cursor, terminal};
    let mut stderr = std::io::stderr();
    stderr.execute(terminal::Clear(terminal::ClearType::All))?;
    stderr.execute(cursor::MoveTo(0, 0))?;
    stderr.flush()?;
    Ok(())
}

#[derive(Debug, Clone)]
struct KokoPrompt {
    graph: String,
    transaction: TransactionMode,
}

impl KokoPrompt {
    fn prefix(&self) -> String {
        let transaction = match self.transaction {
            TransactionMode::None => "",
            TransactionMode::ReadOnly => "|ro-tx",
            TransactionMode::ReadWrite => "|tx",
            _ => "",
        };
        format!("koko[{}{transaction}]", self.graph)
    }

    fn text(&self) -> String {
        format!("{}> ", self.prefix())
    }
}

fn prompt_for(runner: &SourceRunner<'_>) -> KokoPrompt {
    runner.session_snapshot().map_or(
        KokoPrompt {
            graph: "?".to_string(),
            transaction: TransactionMode::None,
        },
        |session| KokoPrompt {
            graph: session.graph().name().to_string(),
            transaction: session.transaction(),
        },
    )
}

impl Prompt for KokoPrompt {
    fn render_prompt_left(&self) -> Cow<'_, str> {
        Cow::Owned(self.prefix())
    }

    fn render_prompt_right(&self) -> Cow<'_, str> {
        Cow::Borrowed("")
    }

    fn render_prompt_indicator(&self, _prompt_mode: PromptEditMode) -> Cow<'_, str> {
        Cow::Borrowed("> ")
    }

    fn render_prompt_multiline_indicator(&self) -> Cow<'_, str> {
        Cow::Borrowed("...> ")
    }

    fn render_prompt_history_search_indicator(&self, search: PromptHistorySearch) -> Cow<'_, str> {
        let label = match search.status {
            PromptHistorySearchStatus::Passing => "bck-i-search",
            PromptHistorySearchStatus::Failing => "failing bck-i-search",
        };
        Cow::Owned(format!("{label}: {}_", search.term))
    }
}

struct ConfirmationPrompt;

impl Prompt for ConfirmationPrompt {
    fn render_prompt_left(&self) -> Cow<'_, str> {
        Cow::Borrowed("Clear history? [y/N]")
    }

    fn render_prompt_right(&self) -> Cow<'_, str> {
        Cow::Borrowed("")
    }

    fn render_prompt_indicator(&self, _prompt_mode: PromptEditMode) -> Cow<'_, str> {
        Cow::Borrowed(" ")
    }

    fn render_prompt_multiline_indicator(&self) -> Cow<'_, str> {
        Cow::Borrowed(" ")
    }

    fn render_prompt_history_search_indicator(&self, _search: PromptHistorySearch) -> Cow<'_, str> {
        Cow::Borrowed(" ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> DynamicEditorState {
        DynamicEditorState {
            completion: true,
            highlighting: true,
            multiline: true,
            parameters: vec!["min_age".to_string()],
            catalog: CompletionCatalog {
                graphs: vec![Candidate::new(
                    "analytics",
                    "graph",
                    Some("typed".into()),
                    1,
                )],
                node_labels: vec![Candidate::new("Person", "node label", None, 1)],
                relationship_labels: vec![Candidate::new("Knows", "relationship label", None, 1)],
                properties: vec![Candidate::new(
                    "name",
                    "property",
                    Some("STRING  Person".into()),
                    1,
                )],
                functions: vec![Candidate::new(
                    "count",
                    "aggregate",
                    Some("count(ANY) -> INT64".into()),
                    2,
                )],
                settings: vec![Candidate::new(
                    "threads",
                    "setting",
                    Some("INT64".into()),
                    1,
                )],
            },
        }
    }

    #[test]
    fn canonical_contexts_drive_completion_families() {
        let state = state();
        let cases = [
            ("USE GRAPH ana", "analytics"),
            ("MATCH (n:Per", "Person"),
            ("MATCH ()-[r:Kno", "Knows"),
            ("MATCH (n) RETURN n.na", "name"),
            ("RETURN $min", "$min_age"),
            ("CALL thr", "threads"),
        ];
        for (line, expected) in cases {
            let values = complete_cypher(line, line.len(), &state)
                .into_iter()
                .map(|suggestion| suggestion.value)
                .collect::<Vec<_>>();
            assert!(
                values.iter().any(|value| value == expected),
                "{line}: {values:?}"
            );
        }
    }

    #[test]
    fn command_keyword_variable_function_and_path_completion_are_covered() {
        let state = state();
        let command = complete_meta(":for", 4, &state);
        assert!(command.iter().any(|item| item.value == ":format"));
        let keyword = complete_cypher("RET", 3, &state);
        assert!(keyword.iter().any(|item| item.value == "RETURN"));
        let variable = complete_cypher("MATCH (person) RETURN per", 25, &state);
        assert!(variable.iter().any(|item| item.value == "person"));
        let function = complete_cypher("RETURN cou(", 10, &state);
        assert!(function.iter().any(|item| item.value == "count"));

        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("lady file.cypher");
        std::fs::write(&path, "RETURN 1").unwrap();
        let prefix = root.path().join("lady").to_string_lossy().into_owned();
        let paths = complete_path_candidates(&prefix);
        assert!(
            paths
                .iter()
                .any(|item| item.value.ends_with("lady file.cypher"))
        );
    }

    #[test]
    fn documented_editor_keys_are_bound() {
        let bindings = koko_keybindings();
        let keys = [
            (KeyModifiers::CONTROL, KeyCode::Char('a')),
            (KeyModifiers::NONE, KeyCode::Home),
            (KeyModifiers::CONTROL, KeyCode::Char('e')),
            (KeyModifiers::NONE, KeyCode::End),
            (KeyModifiers::CONTROL, KeyCode::Home),
            (KeyModifiers::CONTROL, KeyCode::End),
            (KeyModifiers::CONTROL, KeyCode::Char('b')),
            (KeyModifiers::NONE, KeyCode::Left),
            (KeyModifiers::CONTROL, KeyCode::Char('f')),
            (KeyModifiers::NONE, KeyCode::Right),
            (KeyModifiers::ALT, KeyCode::Char('b')),
            (KeyModifiers::ALT, KeyCode::Char('f')),
            (KeyModifiers::NONE, KeyCode::Backspace),
            (KeyModifiers::CONTROL, KeyCode::Char('h')),
            (KeyModifiers::NONE, KeyCode::Delete),
            (KeyModifiers::CONTROL, KeyCode::Char('d')),
            (KeyModifiers::CONTROL, KeyCode::Char('w')),
            (KeyModifiers::ALT, KeyCode::Backspace),
            (KeyModifiers::CONTROL, KeyCode::Char('u')),
            (KeyModifiers::CONTROL, KeyCode::Char('k')),
            (KeyModifiers::CONTROL, KeyCode::Char('t')),
            (KeyModifiers::CONTROL, KeyCode::Char('l')),
            (KeyModifiers::CONTROL, KeyCode::Char('p')),
            (KeyModifiers::NONE, KeyCode::Up),
            (KeyModifiers::CONTROL, KeyCode::Char('n')),
            (KeyModifiers::NONE, KeyCode::Down),
            (KeyModifiers::CONTROL, KeyCode::Char('r')),
            (KeyModifiers::NONE, KeyCode::Tab),
            (KeyModifiers::SHIFT, KeyCode::BackTab),
            (KeyModifiers::CONTROL, KeyCode::Char('g')),
            (KeyModifiers::CONTROL, KeyCode::Char('c')),
            (KeyModifiers::ALT, KeyCode::Enter),
            (KeyModifiers::CONTROL, KeyCode::Char('j')),
        ];
        for (modifier, key) in keys {
            assert!(
                bindings.find_binding(modifier, key).is_some(),
                "missing binding for {modifier:?} {key:?}"
            );
        }
    }

    #[test]
    fn strings_and_comments_do_not_offer_symbols() {
        let state = state();
        assert!(complete_cypher("RETURN 'Per'", 11, &state).is_empty());
        assert!(complete_cypher("RETURN 1 // Per", 15, &state).is_empty());
    }

    #[test]
    fn validator_uses_canonical_multiline_state() {
        let settings = Settings::defaults(true);
        let shared = SharedEditorState(Arc::new(RwLock::new(DynamicEditorState::new(
            &settings, true,
        ))));
        let validator = KokoValidator(shared.clone());
        assert!(matches!(
            validator.validate("MATCH (n"),
            ValidationResult::Incomplete
        ));
        shared.write().multiline = false;
        assert!(matches!(
            validator.validate("MATCH (n"),
            ValidationResult::Complete
        ));
    }

    #[test]
    fn validator_honors_only_lexically_scoped_continuation_markers() {
        let settings = Settings::defaults(true);
        let shared = SharedEditorState(Arc::new(RwLock::new(DynamicEditorState::new(
            &settings, true,
        ))));
        let validator = KokoValidator(shared.clone());
        assert!(matches!(
            validator.validate("MATCH (n) \\"),
            ValidationResult::Incomplete
        ));
        assert!(matches!(
            validator.validate("MATCH (n) \\\nCREATE (n)-[:SELF]->(n);"),
            ValidationResult::Complete
        ));
        assert!(matches!(
            validator.validate("RETURN 1 // \\"),
            ValidationResult::Complete
        ));
        assert!(matches!(
            validator.validate(":read C:\\"),
            ValidationResult::Complete
        ));

        shared.write().multiline = false;
        assert!(matches!(
            validator.validate("RETURN 1 \\"),
            ValidationResult::Incomplete
        ));
    }

    #[test]
    fn highlighting_preserves_utf8_and_tracks_dynamic_toggles() {
        let shared = SharedEditorState(Arc::new(RwLock::new(state())));
        let highlighter = KokoHighlighter(shared.clone());
        let source = "RETURN '東京👩‍💻' AS value // note";
        let styled = highlighter.highlight(source, source.len());
        assert_eq!(
            styled
                .buffer
                .iter()
                .map(|(_, text)| text.as_str())
                .collect::<String>(),
            source
        );
        assert!(styled.buffer.iter().any(|(style, text)| {
            text == "RETURN" && style.foreground == Some(Color::Blue) && style.is_bold
        }));
        assert!(styled.buffer.iter().any(|(style, text)| {
            text == "'東京👩‍💻'" && style.foreground == Some(Color::Green)
        }));
        shared.write().highlighting = false;
        let plain = highlighter.highlight(source, source.len());
        assert_eq!(plain.buffer, vec![(Style::new(), source.to_string())]);
    }

    #[test]
    fn completion_toggle_takes_effect_without_rebuilding_the_editor() {
        let shared = SharedEditorState(Arc::new(RwLock::new(state())));
        let mut completer = KokoCompleter(shared.clone());
        assert!(!completer.complete("RET", 3).is_empty());
        shared.write().completion = false;
        assert!(completer.complete("RET", 3).is_empty());
    }

    #[test]
    fn pasted_tabs_preserve_string_literals_only() {
        assert_eq!(normalize_pasted_tabs("RETURN\t'a\tb'"), "RETURN    'a\tb'");
    }
}
