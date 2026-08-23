use super::*;

#[test]
fn error_with_column_caret() {
    let stderr = "Broken.java:8: error: incompatible types: String cannot be converted to int\n        int x = \"hello\";\n                ^\n1 error\n";
    let diags = parse_stderr(stderr);
    assert_eq!(diags.len(), 1);
    let d = &diags[0];
    assert_eq!(d.path, "Broken.java");
    assert_eq!(d.line, 8);
    assert_eq!(d.severity, DiagnosticSeverity::ERROR);
    assert_eq!(
        d.message,
        "incompatible types: String cannot be converted to int"
    );
    assert_eq!(d.column, Some(16));
    assert_eq!(d.end_character, Some(24)); // "        int x = "hello";".len() in UTF-16 units
}

#[test]
fn warning_is_reported_with_warning_severity() {
    let stderr = "Broken.java:7: warning: [unchecked] unchecked call to add(E) as a member of the raw type List\n        list.add(1);\n                ^\n1 warning\n";
    let diags = parse_stderr(stderr);
    assert_eq!(diags.len(), 1);
    assert_eq!(diags[0].severity, DiagnosticSeverity::WARNING);
    assert_eq!(
        diags[0].message,
        "[unchecked] unchecked call to add(E) as a member of the raw type List"
    );
}

#[test]
fn multi_line_message_folds_continuation_lines_in() {
    let stderr = "Broken.java:9: error: cannot find symbol\n        System.out.println(foo);\n                           ^\n  symbol:   variable foo\n  location: class Broken\nNote: Broken.java uses unchecked or unsafe operations.\n1 error\n";
    let diags = parse_stderr(stderr);
    assert_eq!(diags.len(), 1);
    assert_eq!(
        diags[0].message,
        "cannot find symbol\n  symbol:   variable foo\n  location: class Broken"
    );
}

#[test]
fn note_lines_are_ignored_and_do_not_break_parsing() {
    let stderr = "Broken.java:8: error: incompatible types: String cannot be converted to int\n        int x = \"hello\";\n                ^\nNote: Broken.java uses unchecked or unsafe operations.\nNote: Recompile with -Xlint:unchecked for details.\n1 error\n";
    let diags = parse_stderr(stderr);
    assert_eq!(diags.len(), 1);
    assert_eq!(
        diags[0].message,
        "incompatible types: String cannot be converted to int"
    );
}

#[test]
fn path_with_spaces_parses_correctly() {
    let stderr = "My File.java:9: error: cannot find symbol\n        System.out.println(foo);\n                           ^\n  symbol:   variable foo\n  location: class Broken\n1 error\n";
    let diags = parse_stderr(stderr);
    assert_eq!(diags.len(), 1);
    assert_eq!(diags[0].path, "My File.java");
    assert_eq!(diags[0].line, 9);
}

#[test]
fn xmaxerrs_truncation_notice_is_skipped_not_a_diagnostic() {
    let stderr = "Many.java:2: error: incompatible types: String cannot be converted to int\n    void m0() { int x0 = \"bad0\"; }\n                         ^\nMany.java:3: error: incompatible types: String cannot be converted to int\n    void m1() { int x1 = \"bad1\"; }\n                         ^\nonly showing the first 2 errors, of 20 total; use -Xmaxerrs if you would like to see more\n";
    let diags = parse_stderr(stderr);
    assert_eq!(diags.len(), 2);
    assert_eq!(diags[0].line, 2);
    assert_eq!(diags[1].line, 3);
}

#[test]
fn malformed_lines_are_skipped_without_panicking() {
    let stderr = "garbage line with no colon-marker\n:::: also garbage ::::\nBroken.java:8: error: incompatible types: String cannot be converted to int\n        int x = \"hello\";\n                ^\ntrailing garbage\n1 error\n";
    let diags = parse_stderr(stderr);
    assert_eq!(diags.len(), 1);
    assert_eq!(diags[0].line, 8);
}

#[test]
fn empty_stderr_yields_no_diagnostics() {
    assert_eq!(parse_stderr(""), Vec::new());
}

#[test]
fn group_diagnostics_tags_source_javac_and_groups_by_path() {
    let raw = vec![
        RawDiagnostic {
            path: "A.java".to_string(),
            line: 3,
            severity: DiagnosticSeverity::ERROR,
            message: "boom".to_string(),
            column: Some(4),
            end_character: Some(10),
        },
        RawDiagnostic {
            path: "A.java".to_string(),
            line: 5,
            severity: DiagnosticSeverity::WARNING,
            message: "meh".to_string(),
            column: None,
            end_character: None,
        },
    ];
    let grouped = group_diagnostics(raw);
    assert_eq!(grouped.len(), 1);
    let diags = &grouped["A.java"];
    assert_eq!(diags.len(), 2);
    assert_eq!(diags[0].source.as_deref(), Some("javac"));
    assert_eq!(
        diags[0].range,
        Range {
            start: Position {
                line: 2,
                character: 4
            },
            end: Position {
                line: 2,
                character: 10
            }
        }
    );
    assert_eq!(
        diags[1].range.start,
        Position {
            line: 4,
            character: 0
        }
    );
    assert_eq!(diags[1].range.end.character, u32::MAX);
}

#[test]
fn count_severities_counts_errors_and_warnings_separately() {
    let stderr = "A.java:1: error: e1\n^\nA.java:2: warning: w1\n^\nA.java:3: error: e2\n^\n2 errors\n1 warning\n";
    let diags = parse_stderr(stderr);
    assert_eq!(count_severities(&diags), (2, 1));
}

#[test]
fn source_level_args_enable_preview_at_detected_version() {
    assert_eq!(
        source_level_args(Some(21)),
        vec!["-source", "21", "-target", "21", "--enable-preview"]
    );
    // Undetectable version keeps the previous bare-compile behavior.
    assert!(source_level_args(None).is_empty());
}

#[test]
fn clamp_timeout_secs_clamps_to_bounds() {
    assert_eq!(clamp_timeout_secs(None), 120);
    assert_eq!(clamp_timeout_secs(Some(1)), 10);
    assert_eq!(clamp_timeout_secs(Some(10_000)), 600);
    assert_eq!(clamp_timeout_secs(Some(30)), 30);
}

#[test]
fn locate_javac_override_wins_and_must_itself_be_valid() {
    let dir = std::env::temp_dir().join(format!("jvl-javac-locate-test-{}", std::process::id()));
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let exe_name = if cfg!(windows) { "javac.exe" } else { "javac" };
    std::fs::write(bin.join(exe_name), b"").unwrap();

    assert_eq!(locate_javac(Some(&dir)), Some(bin.join(exe_name)));

    // A bogus override must not silently fall back to $JAVA_HOME.
    assert_eq!(locate_javac(Some(Path::new("/nonexistent/jdk/home"))), None);

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn collect_source_files_skips_build_output_and_hidden_dirs() {
    let dir = std::env::temp_dir().join(format!("jvl-javac-collect-test-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::create_dir_all(dir.join("target")).unwrap();
    std::fs::create_dir_all(dir.join(".git")).unwrap();
    std::fs::create_dir_all(dir.join(".hidden")).unwrap();
    std::fs::write(dir.join("src/Good.java"), "class Good {}").unwrap();
    std::fs::write(dir.join("target/Ignored.java"), "class Ignored {}").unwrap();
    std::fs::write(dir.join(".git/Ignored2.java"), "class Ignored2 {}").unwrap();
    std::fs::write(dir.join(".hidden/Ignored3.java"), "class Ignored3 {}").unwrap();

    let files = collect_source_files(std::slice::from_ref(&dir));
    assert_eq!(files, vec![dir.join("src/Good.java")]);

    std::fs::remove_dir_all(&dir).ok();
}

/// A `sh` fixture standing in for `javac`: its *first* statement forks
/// a background `sleep 10` that inherits (and keeps holding) the
/// stdout/stderr pipes — the "pipe-holding orphan" a killed child can
/// leave behind — then sleeps far longer than the configured timeout,
/// then (only if it ever gets there) touches `marker` — so "the marker
/// never appears" proves the child was actually killed, not merely that
/// `run` gave up waiting on it. Backgrounding the pipe-holder first
/// thing (rather than relying on the foreground `sleep`'s own fork)
/// keeps the leaked-reader assertions deterministic: the kill only has
/// to land after the script's very first statement, and the tests give
/// that a full second of margin. Unix-only (a shell script fixture);
/// the timeout path itself is exercised here since racing a real
/// multi-second `javac` invocation in CI would be slow and flaky —
/// everything else about process hygiene (kill + reap, no zombie) is
/// covered by inspection in `poll_until_exit_or_timeout` and these
/// tests' own promptness/marker assertions.
#[cfg(unix)]
fn write_slow_fake_javac(dir: &Path, marker: &Path, sleep_secs: u64) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let script = dir.join("fake-javac.sh");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nsleep 10 &\nsleep {sleep_secs}\ntouch \"{}\"\n",
            marker.display()
        ),
    )
    .unwrap();
    let mut perms = std::fs::metadata(&script).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&script, perms).unwrap();
    script
}

#[cfg(unix)]
#[test]
fn run_kills_and_reaps_on_timeout() {
    let dir = std::env::temp_dir().join(format!("jvl-javac-timeout-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let marker = dir.join("ran-too-long");
    let script = write_slow_fake_javac(&dir, &marker, 5);

    let config = RunConfig {
        javac_path: script,
        source_files: vec![],
        classpath_entries: vec![],
        timeout: Duration::from_secs(1),
        source_release: None,
    };
    let slot: SharedChild = Arc::new(StdMutex::new(None));
    let leaked = LeakedReaders::new();
    let start = Instant::now();
    let outcome = run(config, &slot, &leaked);
    let elapsed = start.elapsed();

    assert!(matches!(outcome, RunOutcome::TimedOut));
    assert!(
        elapsed < Duration::from_secs(3),
        "should have been killed well before the fake script's 5s sleep: {elapsed:?}"
    );
    // The backgrounded `sleep 10` orphan still holds both pipes open,
    // so both abandoned reader threads must be accounted for.
    assert_eq!(
        leaked.live_count(),
        2,
        "both abandoned reader threads should be counted as leaked"
    );
    // Give a killed-but-somehow-still-running process a moment, then
    // confirm it never reached the `touch` — i.e. it was truly killed.
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        !marker.exists(),
        "child should have been killed before reaching the marker touch"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[cfg(unix)]
#[test]
fn kill_running_child_cancels_an_in_flight_run() {
    let dir = std::env::temp_dir().join(format!("jvl-javac-shutdown-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let marker = dir.join("ran-too-long");
    let script = write_slow_fake_javac(&dir, &marker, 5);

    let config = RunConfig {
        javac_path: script,
        source_files: vec![],
        classpath_entries: vec![],
        timeout: Duration::from_secs(30), // long enough that only the kill ends it
        source_release: None,
    };
    let slot: SharedChild = Arc::new(StdMutex::new(None));
    let run_slot = Arc::clone(&slot);
    let start = Instant::now();
    let handle = std::thread::spawn(move || run(config, &run_slot, &LeakedReaders::new()));

    // Give `run` time to spawn the fake javac and store it in `slot`
    // (it sleeps 5s before touching anything, so this is comfortably
    // before it would exit on its own).
    std::thread::sleep(Duration::from_millis(300));
    kill_running_child(&slot); // simulates server shutdown mid-check.

    let outcome = handle.join().expect("run thread panicked");
    let elapsed = start.elapsed();
    assert!(matches!(outcome, RunOutcome::Cancelled));
    assert!(
        elapsed < Duration::from_secs(3),
        "should have ended promptly on kill, not waited for the 30s timeout: {elapsed:?}"
    );
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        !marker.exists(),
        "child should have been killed before reaching the marker touch"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn leaked_readers_sweep_completed_flags() {
    let leaked = LeakedReaders::with_cap(8);
    let still_blocked = Arc::new(AtomicBool::new(false));
    let finished = Arc::new(AtomicBool::new(false));
    leaked.register(Arc::clone(&still_blocked));
    leaked.register(Arc::clone(&finished));
    assert_eq!(leaked.live_count(), 2);

    // A leaked thread finally unblocking (its flag set) frees capacity
    // on the next sweep — the refusal is self-healing, not permanent.
    finished.store(true, Ordering::SeqCst);
    assert_eq!(leaked.live_count(), 1);
    still_blocked.store(true, Ordering::SeqCst);
    assert_eq!(leaked.live_count(), 0);
}

/// The refusal half of the bounded-leak invariant, deterministically:
/// with the still-blocked count at a (test-lowered) cap, [`run`] must
/// refuse outright — before creating the scratch dir or spawning
/// anything — with a clear, actionable error, so a hostile `jdk.home`
/// binary can't grow blocked threads without bound. The leak-*counting*
/// half (a timed-out run registers its abandoned readers) is covered by
/// `run_kills_and_reaps_on_timeout`'s `live_count` assertion; here the
/// flags are registered by hand instead of racing real processes, which
/// keeps the test load-independent.
#[test]
fn leaked_reader_cap_refuses_further_runs() {
    let leaked = LeakedReaders::with_cap(2);
    leaked.register(Arc::new(AtomicBool::new(false)));
    leaked.register(Arc::new(AtomicBool::new(false)));

    let slot: SharedChild = Arc::new(StdMutex::new(None));
    let outcome = run(
        RunConfig {
            // Never reached: the refusal must precede any spawn attempt.
            javac_path: PathBuf::from("/nonexistent/javac-never-spawned"),
            source_files: vec![],
            classpath_entries: vec![],
            timeout: Duration::from_millis(200),
            source_release: None,
        },
        &slot,
        &leaked,
    );
    match outcome {
        RunOutcome::SpawnError(msg) => {
            assert!(
                msg.contains("leaking resources") && msg.contains("jdk.home"),
                "refusal message should be clear and actionable: {msg}"
            );
        }
        _ => panic!("expected the run to be refused at the leak cap"),
    }

    // One leaked thread unblocking frees capacity: cap 2 with only 1
    // still blocked runs again (and immediately fails to spawn the
    // nonexistent binary — proving it got *past* the refusal gate).
    let leaked = LeakedReaders::with_cap(2);
    leaked.register(Arc::new(AtomicBool::new(false)));
    let outcome = run(
        RunConfig {
            javac_path: PathBuf::from("/nonexistent/javac-never-spawned"),
            source_files: vec![],
            classpath_entries: vec![],
            timeout: Duration::from_millis(200),
            source_release: None,
        },
        &slot,
        &leaked,
    );
    match outcome {
        RunOutcome::SpawnError(msg) => {
            assert!(
                msg.contains("failed to spawn javac"),
                "below the cap the run must proceed to the (failing) spawn: {msg}"
            );
        }
        _ => panic!("expected a spawn failure, not a refusal, below the cap"),
    }
}

/// An entry `std::env::join_paths` can't
/// represent (it contains the platform's path-list separator) must fail
/// the run loudly, naming the offending entry — never silently drop
/// `-cp` and let the user chase misleading cannot-find-symbol
/// diagnostics.
#[cfg(unix)]
#[test]
fn classpath_entry_with_separator_fails_loud() {
    let config = RunConfig {
        javac_path: PathBuf::from("/nonexistent/javac-never-spawned"),
        source_files: vec![],
        classpath_entries: vec![
            PathBuf::from("/deps/fine.jar"),
            PathBuf::from("/deps/evil:name.jar"),
        ],
        timeout: Duration::from_secs(10),
        source_release: None,
    };
    let slot: SharedChild = Arc::new(StdMutex::new(None));
    let outcome = run(config, &slot, &LeakedReaders::new());
    match outcome {
        RunOutcome::SpawnError(msg) => {
            assert!(
                msg.contains("/deps/evil:name.jar"),
                "the offending entry must be named: {msg}"
            );
            assert!(msg.contains("separator"), "the cause must be stated: {msg}");
        }
        _ => panic!("expected a loud SpawnError for the unjoinable classpath entry"),
    }
}
