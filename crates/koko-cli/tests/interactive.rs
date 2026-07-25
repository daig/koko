use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::io::{Read, Write};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

struct PtySession {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    master: Box<dyn MasterPty + Send>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    output: Arc<Mutex<Vec<u8>>>,
    reader: Option<thread::JoinHandle<()>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
fn history_path(root: &std::path::Path) -> std::path::PathBuf {
    #[cfg(target_os = "macos")]
    {
        root.join("Library/Application Support/Koko/history")
    }
    #[cfg(target_os = "windows")]
    {
        root.join("Koko/history")
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        root.join("koko/history")
    }
}

impl PtySession {
    fn start(arguments: &[&str], environment: &[(&str, &str)]) -> Self {
        let mut command = CommandBuilder::new(assert_cmd::cargo::cargo_bin!("koko"));
        command.args(arguments);
        Self::start_command(command, environment)
    }

    #[cfg(unix)]
    fn start_shell(script: &str, environment: &[(&str, &str)]) -> Self {
        let mut command = CommandBuilder::new("/bin/sh");
        command.args(["-c", script]);
        command.env("KOKO_BIN", assert_cmd::cargo::cargo_bin!("koko"));
        Self::start_command(command, environment)
    }

    fn start_command(mut command: CommandBuilder, environment: &[(&str, &str)]) -> Self {
        command.env("TERM", "xterm-256color");
        command.env("NO_COLOR", "1");
        #[cfg(target_os = "windows")]
        if let Some((_, home)) = environment.iter().find(|(name, _)| name == &"HOME") {
            command.env("USERPROFILE", home);
            command.env(
                "APPDATA",
                std::path::Path::new(home).join("AppData/Roaming"),
            );
            command.env("LOCALAPPDATA", home);
        }
        for (name, value) in environment {
            if name == &"NO_COLOR" && value.is_empty() {
                command.env_remove(name);
            } else {
                command.env(name, value);
            }
        }
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 30,
                cols: 100,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let child = pair.slave.spawn_command(command).unwrap();
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().unwrap();
        let writer = Arc::new(Mutex::new(pair.master.take_writer().unwrap()));
        let reader_writer = Arc::clone(&writer);
        let output = Arc::new(Mutex::new(Vec::new()));
        let reader_output = Arc::clone(&output);
        let reader_thread = thread::spawn(move || {
            let mut buffer = [0_u8; 4096];
            loop {
                let Ok(count) = reader.read(&mut buffer) else {
                    break;
                };
                if count == 0 {
                    break;
                }
                let chunk = &buffer[..count];
                {
                    let mut output = lock(&reader_output);
                    let available = (1024 * 1024_usize).saturating_sub(output.len());
                    output.extend_from_slice(&chunk[..chunk.len().min(available)]);
                }
                for _ in chunk.windows(4).filter(|window| *window == b"\x1b[6n") {
                    let mut writer = lock(&reader_writer);
                    let _ = writer.write_all(b"\x1b[1;1R");
                    let _ = writer.flush();
                }
            }
        });
        Self {
            child,
            master: pair.master,
            writer,
            output,
            reader: Some(reader_thread),
        }
    }

    fn send(&self, bytes: &[u8]) {
        let mut writer = lock(&self.writer);
        writer.write_all(bytes).unwrap();
        writer.flush().unwrap();
    }

    #[cfg(unix)]
    fn interrupt_process(&self) {
        let process_id = self.child.process_id().expect("PTY child process id");
        let status = std::process::Command::new("kill")
            .args(["-INT", &process_id.to_string()])
            .status()
            .unwrap();
        assert!(status.success());
    }

    fn resize(&self, rows: u16, cols: u16) {
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
    }
    fn mark(&self) -> usize {
        lock(&self.output).len()
    }

    fn wait_for_after(&self, mark: usize, needle: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let output = lock(&self.output);
            let start = mark.min(output.len());
            let text = String::from_utf8_lossy(&output[start..]).into_owned();
            drop(output);
            if text.contains(needle) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "missing {needle:?} after output offset {mark}:\n{text}"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn wait_for(&self, needle: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let text = String::from_utf8_lossy(&lock(&self.output)).into_owned();
            if text.contains(needle) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "missing {needle:?} in PTY output:\n{text}"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn finish(mut self, expected_code: u32) -> String {
        let deadline = Instant::now() + Duration::from_secs(10);
        let status = loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                let text = String::from_utf8_lossy(&lock(&self.output)).into_owned();
                self.child.kill().unwrap();
                panic!("PTY child did not exit:\n{text}");
            }
            thread::sleep(Duration::from_millis(10));
        };
        let text = String::from_utf8_lossy(&lock(&self.output)).into_owned();
        assert_eq!(
            status.exit_code(),
            expected_code,
            "PTY child status: {status:?}\n{text}"
        );
        drop(self.writer);
        if let Some(reader) = self.reader.take() {
            reader.join().unwrap();
        }
        text
    }
}

#[test]
fn real_pty_greeting_multiline_unicode_graph_and_transaction_prompts() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().to_string_lossy();
    let session = PtySession::start(
        &["--no-config", "--no-history"],
        &[("HOME", home.as_ref()), ("XDG_STATE_HOME", home.as_ref())],
    );
    session.wait_for("Koko");
    session.wait_for("koko[main]>");

    session.send(b"RETURN (\r");
    session.wait_for("...>");
    session.send(&[0x0a]); // Ctrl-J: force submit the incomplete buffer.
    session.wait_for("Parser exception:");

    session.send("RETURN '東京👩‍💻' AS text\r".as_bytes());
    session.wait_for("東京👩‍💻");
    session.send(b"CREATE GRAPH analytics ANY\rUSE GRAPH analytics\r");
    session.wait_for("koko[analytics]>");
    session.send(b"BEGIN TRANSACTION READ ONLY\r");
    session.wait_for("koko[analytics|ro-tx]>");
    session.send(b"ROLLBACK\r:quit\r");
    let output = session.finish(0);
    assert!(output.contains("koko[analytics]>"));
}

#[test]
fn real_pty_completion_and_quiet_mode_remain_functional() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().to_string_lossy();
    let session = PtySession::start(
        &["--no-config", "--no-history", "--quiet"],
        &[("HOME", home.as_ref()), ("XDG_STATE_HOME", home.as_ref())],
    );
    session.wait_for("koko[main]>");
    session.send(b"RET\t");
    session.wait_for("RETURN");
    session.send(b"1 AS answer\r");
    session.wait_for("answer");
    session.send(b":quit\r");
    let output = session.finish(0);
    assert!(!output.contains("Type :help"));
    assert!(output.contains("answer"));
}

#[test]
fn real_pty_parameters_multiple_results_and_command_errors_recover() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().to_string_lossy();
    let session = PtySession::start(
        &["--no-config", "--no-history", "--quiet"],
        &[("HOME", home.as_ref()), ("XDG_STATE_HOME", home.as_ref())],
    );
    session.wait_for("koko[main]>");
    session.send(b":param answer 41\r");
    session.wait_for("Parameter $answer set.");
    session.send(b"RETURN $answer + 1 AS parameter\r");
    session.wait_for("42");
    session.send(b":param answer not-json\r");
    session.wait_for("invalid JSON value");
    session.send(b":not-a-command\r");
    session.wait_for("unknown meta command");
    session.send(b"RETURN 1 AS first; RETURN 2 AS second\r");
    session.wait_for("Result 2 of 2");
    session.send(b":param clear answer\r:quit\r");
    let output = session.finish(0);
    assert!(output.contains("Result 1 of 2"));
    assert!(output.contains("first"));
    assert!(output.contains("second"));
}

#[test]
fn real_pty_history_admission_persistence_and_confirmed_clear() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().to_string_lossy();
    let environment = [
        ("HOME", home.as_ref()),
        ("XDG_STATE_HOME", home.as_ref()),
        ("LOCALAPPDATA", home.as_ref()),
        ("APPDATA", home.as_ref()),
    ];
    let session = PtySession::start(&["--no-config", "--quiet"], &environment);
    session.wait_for("koko[main]>");
    session.send(
        b"RETURN 1 AS kept\r\
          RETURN 1 AS kept\r\
          :param secret \"hidden\"\r\
          :history skip\r\
          RETURN 2 AS skipped\r\
          RETURN 3 AS retained\r\
          :history off\r\
          RETURN 4 AS disabled\r\
          :history on\r\
          :quit\r",
    );
    session.finish(0);

    let path = history_path(root.path());
    let history = std::fs::read_to_string(&path).unwrap();
    assert_eq!(history.matches("RETURN 1 AS kept").count(), 1);
    assert!(history.contains("RETURN 3 AS retained"));
    assert!(!history.contains("hidden"));
    assert!(!history.contains("RETURN 2 AS skipped"));
    assert!(!history.contains("RETURN 4 AS disabled"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    let session = PtySession::start(&["--no-config", "--quiet"], &environment);
    session.wait_for("koko[main]>");
    session.send(b":history clear\r");
    session.wait_for("Clear history? [y/N]");
    session.send(b"y\r:quit\r");
    session.finish(0);
    let history = std::fs::read_to_string(path).unwrap();
    assert_eq!(history.trim(), ":quit");
}

#[test]
fn real_pty_documented_editing_keys_resize_paste_and_search_preserve_input() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().to_string_lossy();
    let session = PtySession::start(
        &["--no-config", "--no-history", "--quiet"],
        &[("HOME", home.as_ref()), ("XDG_STATE_HOME", home.as_ref())],
    );
    session.wait_for("koko[main]>");

    let mut edit = b"RETURN 91 AS value".to_vec();
    edit.extend_from_slice(&[
        0x01, 0x05, // Ctrl-A, Ctrl-E
    ]);
    edit.extend_from_slice(b"\x1b[H\x1b[F\x1b[1;5H\x1b[1;5F"); // Home/End and Ctrl-Home/End.
    edit.extend_from_slice(&[0x02, 0x06]); // Ctrl-B, Ctrl-F.
    edit.extend_from_slice(b"\x1b[D\x1b[C\x1bb\x1bf\x1b[1;3D\x1b[1;3C");
    edit.extend_from_slice(b"xy\x7f\x08"); // Backspace and Ctrl-H.
    edit.extend_from_slice(b"x\x1b[D\x1b[3~"); // Delete.
    edit.extend_from_slice(b"x\x1b[D\x04"); // Ctrl-D with input.
    edit.extend_from_slice(b" junk\x17"); // Ctrl-W.
    edit.extend_from_slice(b" junk\x1b\x7f"); // Alt-Backspace.
    edit.push(0x15); // Ctrl-U.
    edit.extend_from_slice(b"RETURN 91 AS valuejunk\x02\x02\x02\x02\x0b"); // Ctrl-K.
    edit.extend_from_slice(b"ab\x14\x08\x08"); // Ctrl-T followed by cleanup.
    edit.push(0x0c); // Ctrl-L.
    edit.push(b'\r');
    session.send(&edit);
    session.wait_for("rows returned");

    session.send(&[0x10, 0x0e]); // Ctrl-P, Ctrl-N.
    session.send(b"\x1b[A\x1b[B"); // Up, Down.
    session.send(b"junk\x07RETURN 92 AS cleared\r"); // Ctrl-G clears the edit.
    session.wait_for("cleared");
    session.send(b"junk");
    session.wait_for("junk");
    let interrupt_mark = session.mark();
    session.send(&[0x03]); // Ctrl-C silently clears a nonempty edit.
    session.wait_for_after(interrupt_mark, "koko[main]>");
    session.send(b"RETURN 93 AS interrupted\r");
    session.wait_for("interrupted");

    session.resize(12, 24);
    session.send("RETURN '東京👩‍💻x'".as_bytes());
    session.send(b"\x1b[D\x7f\x1b[F AS unicode\r");
    session.wait_for("unicode");

    session.send(b"\x12");
    session.wait_for("bck-i-search");
    session.send(b"91\r\r"); // Accept the search result, then submit it.
    session.wait_for("rows returned");

    session.send(b"\x1b[200~RETURN\t94 AS pasted\x1b[201~\r");
    session.wait_for("pasted");
    session.send(b":quit\r");
    let output = session.finish(0);
    assert!(output.contains("東京👩‍💻"));
    assert!(!output.contains("Press Ctrl-D or :quit to exit"));
}

#[test]
fn real_pty_dumb_terminal_keeps_a_plain_line_prompt() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().to_string_lossy();
    let session = PtySession::start(
        &["--no-config", "--no-history", "--quiet"],
        &[
            ("HOME", home.as_ref()),
            ("XDG_STATE_HOME", home.as_ref()),
            ("TERM", "dumb"),
        ],
    );
    session.wait_for("koko[main]>");
    session.send(b"RETURN 7 AS plain\r:quit\r");
    let output = session.finish(0);
    assert!(output.contains("plain"));
    assert!(!output.contains("\x1b[6n"));
}

#[test]
fn real_pty_explicit_continuation_normalizes_capable_paste_and_dumb_input() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().to_string_lossy();
    let capable = PtySession::start(
        &["--no-config", "--no-history", "--quiet"],
        &[("HOME", home.as_ref()), ("XDG_STATE_HOME", home.as_ref())],
    );
    capable.wait_for("koko[main]>");
    capable.send(b":multiline off\r");
    capable.wait_for("multiline off");
    let continued = capable.mark();
    capable.send(b"UNWIND [1] AS value \\\r");
    capable.wait_for_after(continued, "...>");
    capable.send(b"RETURN value AS continued;\r");
    capable.wait_for_after(continued, "continued");

    let pasted = capable.mark();
    capable
        .send(b"\x1b[200~UNWIND [2] AS value \\\nRETURN value AS pasted_continuation;\x1b[201~\r");
    capable.wait_for_after(pasted, "pasted_continuation");
    capable.send(b":quit\r");
    let capable_output = capable.finish(0);
    assert!(!capable_output.contains("query must either RETURN"));

    let dumb = PtySession::start(
        &["--no-config", "--no-history", "--quiet"],
        &[
            ("HOME", home.as_ref()),
            ("XDG_STATE_HOME", home.as_ref()),
            ("TERM", "dumb"),
        ],
    );
    dumb.wait_for("koko[main]>");
    let continued = dumb.mark();
    dumb.send(b"UNWIND [3] AS value \\\r");
    dumb.wait_for_after(continued, "...>");
    dumb.send(b"RETURN value AS dumb_continuation;\r");
    dumb.wait_for_after(continued, "dumb_continuation");
    dumb.send(b":quit\r");
    let dumb_output = dumb.finish(0);
    assert!(!dumb_output.contains("query must either RETURN"));
}

#[test]
fn real_pty_progress_cancellation_late_signal_and_next_query_are_isolated() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().to_string_lossy();
    let session = PtySession::start(
        &["--no-config", "--no-history", "--quiet"],
        &[("HOME", home.as_ref()), ("XDG_STATE_HOME", home.as_ref())],
    );
    session.wait_for("koko[main]>");
    let cancellation_mark = session.mark();
    session.send(b"UNWIND range(0, 100000000) AS value RETURN sum(value)\r");
    session.wait_for_after(cancellation_mark, "Running…");
    session.send(&[0x03]);
    session.wait_for_after(cancellation_mark, "Cancelling…");
    session.wait_for_after(cancellation_mark, "Query cancelled after");

    let next_mark = session.mark();
    session.send(b"RETURN 42 AS next\r");
    session.wait_for_after(next_mark, "rows returned");
    #[cfg(unix)]
    {
        session.interrupt_process();
        thread::sleep(Duration::from_millis(50));
    }
    let late_mark = session.mark();
    session.send(b"RETURN 43 AS late_safe\r");
    session.wait_for_after(late_mark, "rows returned");
    session.send(b":quit\r");
    let output = session.finish(0);

    let cancelled =
        String::from_utf8_lossy(&output.as_bytes()[cancellation_mark..next_mark.min(output.len())]);
    assert!(cancelled.contains("Query cancelled after"));
    assert!(!cancelled.contains("rows returned"));
    assert!(!cancelled.contains('%'));
    assert!(output.contains("late_safe"));
}

#[test]
fn real_pty_idle_interrupt_exit_and_transaction_protection_follow_policy() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().to_string_lossy();
    let environment = [("HOME", home.as_ref()), ("XDG_STATE_HOME", home.as_ref())];

    let session = PtySession::start(&["--no-config", "--no-history", "--quiet"], &environment);
    session.wait_for("koko[main]>");
    session.send(&[0x03]);
    session.wait_for("Press Ctrl-D or :quit to exit");
    session.send(&[0x03]);
    session.finish(130);

    let session = PtySession::start(&["--no-config", "--no-history", "--quiet"], &environment);
    session.wait_for("koko[main]>");
    session.send(b"BEGIN TRANSACTION\r");
    session.wait_for("koko[main|tx]>");
    session.send(&[0x03]);
    session.wait_for("Press Ctrl-D or :quit to exit");
    let repeated_mark = session.mark();
    session.send(&[0x03]);
    session.wait_for_after(repeated_mark, "Press Ctrl-D or :quit to exit");
    session.send(&[0x04]);
    session.wait_for("Transaction is active. COMMIT, ROLLBACK, or use :quit --rollback.");
    session.send(b":quit --rollback\r");
    session.finish(0);
}

#[cfg(unix)]
#[test]
fn real_pty_terminal_modes_restore_after_query_error_cancellation_and_output_failure() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().to_string_lossy();
    let environment = [("HOME", home.as_ref()), ("XDG_STATE_HOME", home.as_ref())];
    let script = r#"
        trap '' INT
        before=$(stty -g) || exit 90
        "$KOKO_BIN" --no-config --no-history --quiet
        code=$?
        after=$(stty -g) || exit 91
        if [ "$before" = "$after" ]; then
            echo TERMINAL_RESTORED >&2
        else
            echo TERMINAL_NOT_RESTORED >&2
            exit 99
        fi
        exit "$code"
    "#;

    let session = PtySession::start_shell(script, &environment);
    session.wait_for("koko[main]>");
    session.send(b"RETURN (\r");
    session.wait_for("...>");
    session.send(&[0x0a]);
    session.wait_for("Parser exception:");
    session.send(b":quit\r");
    let output = session.finish(0);
    assert!(output.contains("TERMINAL_RESTORED"));
    assert!(!output.contains("TERMINAL_NOT_RESTORED"));

    let session = PtySession::start_shell(script, &environment);
    session.wait_for("koko[main]>");
    session.send(b"UNWIND range(0, 100000000) AS value RETURN sum(value)\r");
    session.wait_for("Running…");
    session.send(&[0x03]);
    session.wait_for("Query cancelled after");
    session.send(b":quit\r");
    let output = session.finish(0);
    assert!(output.contains("TERMINAL_RESTORED"));
    assert!(!output.contains("TERMINAL_NOT_RESTORED"));

    let output_failure_script = r#"
        before=$(stty -g) || exit 90
        "$KOKO_BIN" --no-config --no-history --quiet
        code=$?
        after=$(stty -g) || exit 91
        before_nonlocal=$(printf '%s\n' "$before" | sed 's/lflag=[^:]*/lflag=normalized/')
        after_nonlocal=$(printf '%s\n' "$after" | sed 's/lflag=[^:]*/lflag=normalized/')
        local_modes=$(stty -a) || exit 91
        case "$local_modes" in
            *-icanon*|*-echo\ *) restored=false ;;
            *) restored=true ;;
        esac
        if [ "$before_nonlocal" = "$after_nonlocal" ] && [ "$restored" = true ]; then
            echo TERMINAL_RESTORED >&2
        else
            echo TERMINAL_NOT_RESTORED >&2
            exit 99
        fi
        exit "$code"
    "#;
    let session = PtySession::start_shell(output_failure_script, &environment);
    session.wait_for("koko[main]>");
    session.send(b":output /dev/null replace\r");
    let output = session.finish(1);
    assert!(output.contains("TERMINAL_RESTORED"));
    assert!(!output.contains("TERMINAL_NOT_RESTORED"));
}

#[cfg(unix)]
#[test]
fn real_pty_redirected_stdout_keeps_editor_on_stderr_and_color_is_explicit() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().to_string_lossy();
    let result_path = root.path().join("result.tsv");
    let result = result_path.to_string_lossy();
    let environment = [
        ("HOME", home.as_ref()),
        ("XDG_STATE_HOME", home.as_ref()),
        ("KOKO_OUT", result.as_ref()),
    ];
    let script = r#"
        "$KOKO_BIN" --no-config --no-history --quiet >"$KOKO_OUT"
        exit $?
    "#;
    let session = PtySession::start_shell(script, &environment);
    session.wait_for("koko[main]>");
    session.send(b"RETURN 42 AS redirected\r");
    session.wait_for("rows returned");
    session.send(b":quit\r");
    let output = session.finish(0);
    assert!(output.contains("koko[main]>"));
    assert!(output.contains("rows returned"));
    let result = std::fs::read_to_string(&result_path).unwrap();
    assert_eq!(result, "redirected\n42\n");

    let color_environment = [
        ("HOME", home.as_ref()),
        ("XDG_STATE_HOME", home.as_ref()),
        ("NO_COLOR", ""),
    ];
    let session = PtySession::start(
        &[
            "--no-config",
            "--no-history",
            "--quiet",
            "--color",
            "always",
        ],
        &color_environment,
    );
    session.wait_for("main");
    session.send(b"RET");
    session.send(b"\t1\r:quit\r");
    let output = session.finish(0);
    assert!(output.contains(";34m"));

    let session = PtySession::start(
        &["--no-config", "--no-history", "--quiet", "--color", "never"],
        &color_environment,
    );
    session.wait_for("koko[main]>");
    session.send(b"RETURN 1\r:quit\r");
    let output = session.finish(0);
    assert!(!output.contains(";34m"));

    let session = PtySession::start(&["--no-config", "--no-history", "--quiet"], &environment);
    session.wait_for("koko[main]>");
    session.send(b"RETURN 1\r:quit\r");
    let output = session.finish(0);
    assert!(!output.contains(";34m"));
}
