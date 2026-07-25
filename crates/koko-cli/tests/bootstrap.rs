use koko_cli::bootstrap::{
    BootstrapAction, BootstrapCapabilities, BootstrapFileSystem, ColorChoice, Format, InputMode,
    OutputDestination, PlatformPaths, SettingSource, TerminalProbe,
};
use koko_cli::registry::{COMMAND_REGISTRY, OPTION_REGISTRY};
use koko_cli::value_codec::{decode_parameter_object, decode_value};
use koko_cli::{bootstrap, registry};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
struct Capabilities {
    stdin_terminal: bool,
    stdout_terminal: bool,
    stderr_terminal: bool,
    config: Option<PathBuf>,
    home: Option<PathBuf>,
}

impl TerminalProbe for Capabilities {
    fn stdin_is_terminal(&self) -> bool {
        self.stdin_terminal
    }

    fn stdout_is_terminal(&self) -> bool {
        self.stdout_terminal
    }

    fn stderr_is_terminal(&self) -> bool {
        self.stderr_terminal
    }
}

impl PlatformPaths for Capabilities {
    fn user_config_file(&self) -> Option<PathBuf> {
        self.config.clone()
    }

    fn home_directory(&self) -> Option<PathBuf> {
        self.home.clone()
    }
}

impl BootstrapFileSystem for Capabilities {
    fn read(&self, path: &Path) -> std::io::Result<Vec<u8>> {
        std::fs::read(path)
    }

    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }

    fn is_file(&self, path: &Path) -> bool {
        path.is_file()
    }

    fn parent_is_directory(&self, path: &Path) -> bool {
        path.parent().unwrap_or_else(|| Path::new(".")).is_dir()
    }
}

fn capabilities(root: &Path) -> Capabilities {
    Capabilities {
        stdin_terminal: false,
        stdout_terminal: false,
        stderr_terminal: false,
        config: None,
        home: Some(root.to_path_buf()),
    }
}

fn plan(
    arguments: &[&str],
    capabilities: &impl BootstrapCapabilities,
) -> koko_cli::bootstrap::BootstrapPlan {
    match bootstrap::validate(arguments, capabilities).unwrap() {
        BootstrapAction::Activate(plan) => *plan,
        BootstrapAction::Print(_) => panic!("expected activation"),
    }
}

#[test]
fn command_and_option_registries_are_unique_and_drive_help() {
    let mut commands = HashSet::new();
    for spec in COMMAND_REGISTRY {
        assert!(commands.insert(spec.name));
        assert_eq!(registry::command_spec(spec.name).unwrap().id, spec.id);
    }
    assert_eq!(commands.len(), 22);

    let mut ids = HashSet::new();
    let mut config_keys = HashSet::new();
    let mut cli_names = HashSet::new();
    for spec in OPTION_REGISTRY {
        assert!(ids.insert(spec.id));
        if let Some(key) = spec.config_key {
            assert!(config_keys.insert(key));
        }
        if let Some(long) = spec.long {
            assert!(cli_names.insert(long));
        }
        if let Some(long) = spec.negative_long {
            assert!(cli_names.insert(long));
        }
    }
    assert_eq!(config_keys.len(), 12);
    let help = registry::clap_command().render_long_help().to_string();
    for required in [
        "command",
        "file",
        "param",
        "params-file",
        "init",
        "format",
        "output",
        "force",
        "header",
        "no-header",
        "timing",
        "no-timing",
        "progress",
        "color",
        "null",
        "keep-going",
        "no-config",
        "no-history",
        "quiet",
        "help",
        "version",
    ] {
        assert!(
            help.contains(&format!("--{required}")),
            "missing --{required}"
        );
    }
}

struct PanicCapabilities;

impl TerminalProbe for PanicCapabilities {
    fn stdin_is_terminal(&self) -> bool {
        panic!("help/version must not probe terminals")
    }
    fn stdout_is_terminal(&self) -> bool {
        panic!("help/version must not probe terminals")
    }
    fn stderr_is_terminal(&self) -> bool {
        panic!("help/version must not probe terminals")
    }
}
impl PlatformPaths for PanicCapabilities {
    fn user_config_file(&self) -> Option<PathBuf> {
        panic!("help/version must not locate config")
    }
    fn home_directory(&self) -> Option<PathBuf> {
        panic!("help/version must not locate home")
    }
}
impl BootstrapFileSystem for PanicCapabilities {
    fn read(&self, _: &Path) -> std::io::Result<Vec<u8>> {
        panic!("help/version must not read files")
    }
    fn exists(&self, _: &Path) -> bool {
        panic!("help/version must not inspect files")
    }
    fn is_file(&self, _: &Path) -> bool {
        panic!("help/version must not inspect files")
    }
    fn parent_is_directory(&self, _: &Path) -> bool {
        panic!("help/version must not inspect files")
    }
}

#[test]
fn help_and_version_finish_before_any_capability_or_database_activation() {
    let help = bootstrap::validate(["koko", "--help"], &PanicCapabilities).unwrap();
    assert!(matches!(help, BootstrapAction::Print(text) if text.contains("--command")));
    let version = bootstrap::validate(["koko", "--version"], &PanicCapabilities).unwrap();
    assert!(matches!(version, BootstrapAction::Print(text) if text.contains("Koko engine")));
}

#[test]
fn invocation_errors_are_usage_failures() {
    let root = tempfile::tempdir().unwrap();
    let capabilities = capabilities(root.path());
    for arguments in [
        vec!["koko", "--unknown"],
        vec!["koko", "--format"],
        vec!["koko", "--command", "RETURN 1", "--file", "input.cypher"],
        vec!["koko", "native.db"],
        vec!["koko", "--header", "--no-header"],
    ] {
        let error = bootstrap::validate(arguments, &capabilities).unwrap_err();
        assert_eq!(error.exit_code(), 2);
    }
}

#[test]
fn configuration_errors_name_path_line_key_and_accepted_values() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("config.toml");
    std::fs::write(&path, "format = \"csv\"\nunknown = true\n").unwrap();
    let mut capabilities = capabilities(root.path());
    capabilities.config = Some(path.clone());
    let error = bootstrap::validate(["koko"], &capabilities).unwrap_err();
    let message = error.to_string();
    assert!(
        message.contains(&format!("{}:2", path.display())),
        "{message}"
    );
    assert!(message.contains("`unknown`"));
    assert!(message.contains("accepted:"));

    std::fs::write(&path, "color = \"sometimes\"\n").unwrap();
    let message = bootstrap::validate(["koko"], &capabilities)
        .unwrap_err()
        .to_string();
    assert!(message.contains("`color`"));
    assert!(message.contains("auto, always, never"));
}

#[test]
fn precedence_and_no_config_are_exact_and_narrow() {
    let root = tempfile::tempdir().unwrap();
    let config = root.path().join("config.toml");
    std::fs::write(
        &config,
        "format = \"csv\"\ntiming = false\nprogress = \"on\"\ncolor = \"never\"\nhistory = true\n",
    )
    .unwrap();
    let init = root.path().join("init.cypher");
    std::fs::write(&init, "RETURN 1;\n").unwrap();
    let mut capabilities = capabilities(root.path());
    capabilities.stdin_terminal = true;
    capabilities.config = Some(config.clone());

    let configured_plan = plan(
        &[
            "koko",
            "--format",
            "json",
            "--timing",
            "--color",
            "always",
            "--no-history",
            "--init",
            init.to_str().unwrap(),
        ],
        &capabilities,
    );
    assert_eq!(*configured_plan.settings.format.value(), Format::Json);
    assert_eq!(
        configured_plan.settings.format.source(),
        &SettingSource::CommandLine
    );
    assert!(*configured_plan.settings.timing.value());
    assert_eq!(*configured_plan.settings.color.value(), ColorChoice::Always);
    assert!(!*configured_plan.settings.history.value());
    assert_eq!(
        *configured_plan.settings.progress.value(),
        koko_cli::bootstrap::AutoToggle::On
    );
    assert!(matches!(configured_plan.input, InputMode::Interactive));
    assert!(matches!(
        configured_plan.output,
        OutputDestination::Stdout { terminal: false }
    ));

    std::fs::write(&config, "unknown = true\n").unwrap();
    let plan = plan(
        &["koko", "--no-config", "--init", init.to_str().unwrap()],
        &capabilities,
    );
    assert_eq!(plan.init.as_deref(), Some(init.as_path()));
    assert!(plan.config_path.is_none());
}

#[test]
fn parameter_file_precedes_unique_inline_parameters_and_uses_one_codec() {
    let root = tempfile::tempdir().unwrap();
    let parameters = root.path().join("parameters.json");
    std::fs::write(
        &parameters,
        r#"{"answer":1,"wide":{"$type":"INTEGER","logical_type":"INT128","value":"9007199254740992"},"raw":{"$type":"JSON","value":{"$type":"NODE","x":1}}}"#,
    )
    .unwrap();
    let capabilities = capabilities(root.path());
    let plan = plan(
        &[
            "koko",
            "--params-file",
            parameters.to_str().unwrap(),
            "--param",
            "answer=2",
        ],
        &capabilities,
    );
    assert_eq!(
        plan.parameters.get("answer").unwrap().value(),
        &koko::Value::Int64(2)
    );
    assert!(matches!(
        plan.parameters.get("wide").unwrap().value(),
        koko::Value::IntX {
            value: 9_007_199_254_740_992,
            ..
        }
    ));
    assert!(matches!(
        plan.parameters.get("raw").unwrap().value(),
        koko::Value::Json(_)
    ));

    let error = bootstrap::validate(
        ["koko", "--param", "answer=1", "--param", "answer=2"],
        &capabilities,
    )
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("duplicate command-line parameter")
    );
    assert!(decode_parameter_object(r#"{"x":1,"x":2}"#).is_err());
    assert!(decode_value("9007199254740992").is_err());
}

#[test]
fn explicit_unicode_paths_and_tilde_expansion_are_preserved() {
    let root = tempfile::tempdir().unwrap();
    let input = root.path().join("résumé graph.cypher");
    std::fs::write(&input, "RETURN 1;\n").unwrap();
    let capabilities = capabilities(root.path());
    let plan = plan(&["koko", "--file", "~/résumé graph.cypher"], &capabilities);
    assert_eq!(plan.input, InputMode::File(input));

    let error = bootstrap::validate(["koko", "--file", "https://example.test/a"], &capabilities)
        .unwrap_err();
    assert!(error.to_string().contains("remote path"));
}

#[cfg(unix)]
#[test]
fn explicit_paths_retain_non_utf8_platform_bytes() {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};

    let root = tempfile::tempdir().unwrap();
    let mut bytes = root.path().as_os_str().as_bytes().to_vec();
    bytes.extend_from_slice(b"/output-");
    bytes.push(0xff);
    bytes.extend_from_slice(b".json");
    let output = PathBuf::from(std::ffi::OsString::from_vec(bytes));
    let capabilities = capabilities(root.path());
    let arguments = vec![
        std::ffi::OsString::from("koko"),
        std::ffi::OsString::from("--output"),
        output.as_os_str().to_os_string(),
    ];
    let action = bootstrap::validate(arguments, &capabilities).unwrap();
    let BootstrapAction::Activate(plan) = action else {
        panic!("expected activation");
    };
    assert_eq!(plan.output, OutputDestination::File(output));
}

#[test]
fn bootstrap_never_probes_an_implicit_current_directory_file() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingCapabilities(AtomicUsize);
    impl TerminalProbe for CountingCapabilities {
        fn stdin_is_terminal(&self) -> bool {
            false
        }
        fn stdout_is_terminal(&self) -> bool {
            false
        }
        fn stderr_is_terminal(&self) -> bool {
            false
        }
    }
    impl PlatformPaths for CountingCapabilities {
        fn user_config_file(&self) -> Option<PathBuf> {
            None
        }
        fn home_directory(&self) -> Option<PathBuf> {
            None
        }
    }
    impl BootstrapFileSystem for CountingCapabilities {
        fn read(&self, _: &Path) -> std::io::Result<Vec<u8>> {
            self.0.fetch_add(1, Ordering::Relaxed);
            Ok(Vec::new())
        }
        fn exists(&self, _: &Path) -> bool {
            false
        }
        fn is_file(&self, _: &Path) -> bool {
            false
        }
        fn parent_is_directory(&self, _: &Path) -> bool {
            true
        }
    }

    let capabilities = CountingCapabilities(AtomicUsize::new(0));
    let action = bootstrap::validate(["koko"], &capabilities).unwrap();
    assert!(matches!(action, BootstrapAction::Activate(_)));
    assert_eq!(capabilities.0.load(Ordering::Relaxed), 0);
}

#[test]
fn input_mode_and_output_destination_resolve_independently() {
    let root = tempfile::tempdir().unwrap();
    let mut capabilities = capabilities(root.path());
    capabilities.stdin_terminal = false;
    capabilities.stdout_terminal = true;
    let plan = plan(&["koko"], &capabilities);
    assert_eq!(plan.input, InputMode::PipedStdin);
    assert_eq!(plan.output, OutputDestination::Stdout { terminal: true });
}

#[test]
fn every_documented_configuration_key_is_typed_and_accepted() {
    let root = tempfile::tempdir().unwrap();
    let config = root.path().join("config.toml");
    std::fs::write(
        &config,
        r#"
format = "auto"
timing = true
progress = "auto"
color = "auto"
rows = 20
width = "auto"
null_display = "literal"
multiline = true
highlight = "auto"
completion = true
history = true
history_limit = 10000
"#,
    )
    .unwrap();
    let mut capabilities = capabilities(root.path());
    capabilities.config = Some(config.clone());
    let plan = plan(&["koko"], &capabilities);
    assert_eq!(plan.config_path.as_deref(), Some(config.as_path()));
    assert_eq!(*plan.settings.history_limit.value(), 10_000);
    assert!(matches!(
        plan.settings.width.source(),
        SettingSource::Config { line: 7, .. }
    ));
}
