//! Debuggee launch support: JDK binary location, source-root discovery, and
//! debuggee classpath assembly (prebuilt build output, else a one-shot
//! `javac -g` auto-compile into a scratch directory).
//!
//! Mirrors the subprocess-safety template of the server's `javac` check
//! command (those helpers are crate-private to `jvl-server`, so the walk and
//! poll semantics are reimplemented here — not exported): argv-only spawns,
//! `-proc:none` mandatory, bounded output capture, poll/kill/reap timeout.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Build-file names marking a Maven/Gradle module root (same set as the
/// server's `MODULE_MARKERS`; `settings.gradle*` deliberately excluded).
const MODULE_MARKERS: [&str; 3] = ["pom.xml", "build.gradle", "build.gradle.kts"];

/// Conventional source-root subdirectories under a module root.
const CONVENTIONAL_SOURCE_SUBDIRS: [&str; 2] = ["src/main/java", "src/test/java"];

/// Directory names skipped while walking (build output and VCS metadata).
const SKIPPED_DIR_NAMES: [&str; 3] = ["target", "build", ".git"];

/// Cap on directories visited by [`discover_source_roots`]'s walk.
const MAX_MODULE_SCAN_DIRS: usize = 50_000;

/// Cap on `.java` files fed to the auto-compile `javac`.
const MAX_SOURCE_FILES: usize = 10_000;

/// Auto-compile timeout before the child is killed and reaped.
const COMPILE_TIMEOUT: Duration = Duration::from_secs(120);

/// How often the timeout loop polls the child for exit.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Cap on captured `javac` stderr forwarded to the DAP client.
const STDERR_CAP: usize = 64 * 1024;

/// Locate a JDK `bin/<exe>`. The override (extension-injected
/// `__jvlJdkHome`, from the machine-scoped `java-vsix-lite.jdk.home`
/// setting) wins outright when given — a misconfiguration surfaces instead
/// of being masked; else `$JAVA_HOME`; else the classpath layer's
/// filesystem-probing JDK discovery. Never downloads, never spawns.
fn locate_jdk_exe(jdk_home_override: Option<&Path>, exe: &str) -> Option<PathBuf> {
    let exe_name = if cfg!(windows) {
        format!("{exe}.exe")
    } else {
        exe.to_string()
    };
    if let Some(home) = jdk_home_override {
        let candidate = home.join("bin").join(&exe_name);
        return candidate.is_file().then_some(candidate);
    }
    if let Some(home) = std::env::var_os("JAVA_HOME") {
        let candidate = PathBuf::from(home).join("bin").join(&exe_name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    let home = jvl_classpath::best_jdk()?;
    let candidate = home.join("bin").join(&exe_name);
    candidate.is_file().then_some(candidate)
}

/// Locate the `java` launcher (see [`locate_jdk_exe`]).
pub fn locate_java(jdk_home_override: Option<&Path>) -> Option<PathBuf> {
    locate_jdk_exe(jdk_home_override, "java")
}

/// Locate `javac` for the auto-compile fallback (see [`locate_jdk_exe`]).
pub fn locate_javac(jdk_home_override: Option<&Path>) -> Option<PathBuf> {
    locate_jdk_exe(jdk_home_override, "javac")
}

/// Bounded, directory-only walk for every module's conventional source
/// roots under `root` — same semantics as the server's
/// `discover_workspace_source_roots` (skips `target`/`build`/`.git`, hidden
/// dirs, and symlinks; capped at [`MAX_MODULE_SCAN_DIRS`] directories).
pub fn discover_source_roots(root: &Path) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut stack = vec![root.to_path_buf()];
    let mut visited = 0usize;
    while let Some(dir) = stack.pop() {
        if visited >= MAX_MODULE_SCAN_DIRS {
            break;
        }
        visited += 1;
        let is_module = dir == root || MODULE_MARKERS.iter().any(|m| dir.join(m).is_file());
        if is_module {
            for sub in CONVENTIONAL_SOURCE_SUBDIRS {
                let candidate = dir.join(sub);
                if candidate.is_dir() && seen.insert(candidate.clone()) {
                    roots.push(candidate);
                }
            }
        }
        let Ok(read_dir) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in read_dir.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_symlink() || !file_type.is_dir() {
                continue;
            }
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str.starts_with('.') || SKIPPED_DIR_NAMES.contains(&name_str.as_ref()) {
                continue;
            }
            stack.push(entry.path());
        }
    }
    roots
}

/// Walk `roots` for `.java` files (dedup roots; skip build output, hidden
/// dirs, and symlinks; stop at [`MAX_SOURCE_FILES`]).
fn collect_source_files(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut seen_roots = std::collections::HashSet::new();
    for root in roots {
        if !seen_roots.insert(root.clone()) {
            continue;
        }
        let mut stack = vec![root.clone()];
        while let Some(dir) = stack.pop() {
            if out.len() >= MAX_SOURCE_FILES {
                return out;
            }
            let Ok(read_dir) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in read_dir.flatten() {
                if out.len() >= MAX_SOURCE_FILES {
                    return out;
                }
                let Ok(file_type) = entry.file_type() else {
                    continue;
                };
                if file_type.is_symlink() {
                    continue;
                }
                let name = entry.file_name();
                let name_str = name.to_string_lossy();
                if name_str.starts_with('.') || SKIPPED_DIR_NAMES.contains(&name_str.as_ref()) {
                    continue;
                }
                let path = entry.path();
                if file_type.is_dir() {
                    stack.push(path);
                } else if file_type.is_file() && path.extension().is_some_and(|e| e == "java") {
                    out.push(path);
                }
            }
        }
    }
    out
}

/// The assembled debuggee classpath plus the source roots for stack-frame
/// source lookup, and the auto-compile scratch dir (removed best-effort at
/// session end) when one was created.
pub struct ClasspathPlan {
    pub classpath: Vec<PathBuf>,
    pub source_roots: Vec<PathBuf>,
    pub scratch_dir: Option<PathBuf>,
}

/// Why classpath assembly failed. `javac_stderr` is forwarded to the DAP
/// client as a `stderr` output event before the launch error.
#[derive(Debug)]
pub struct ClasspathError {
    pub message: String,
    pub javac_stderr: Option<String>,
}

impl ClasspathError {
    fn new(message: impl Into<String>) -> ClasspathError {
        ClasspathError {
            message: message.into(),
            javac_stderr: None,
        }
    }
}

/// Assemble the debuggee classpath per the decided policy:
/// 1. explicit `classPaths` → used exactly;
/// 2. else existing Maven/Gradle build-output dirs + dependency jars —
///    plus each module's *test* build-output dirs when
///    `include_test_outputs` is set (a test run whose test outputs don't
///    exist yet falls through to 3, which compiles `src/test/java` too);
/// 3. else auto-compile the project's sources with `javac -g` into a
///    scratch dir.
///
/// `additional_class_paths` (the self-contained JUnit console launcher jar
/// for test runs) is inserted verbatim *ahead of* every mode's entries: its
/// classes must win resolution conflicts, or a project depending on a
/// different JUnit version would mix that version's jars with the
/// launcher's aligned classes and die with `NoSuchMethodError` before any
/// test runs. It never overlaps project class dirs, only dependency jars.
///
/// Blocking (filesystem walks, dependency resolution, possibly a `javac`
/// child up to [`COMPILE_TIMEOUT`]) — call via `spawn_blocking`.
pub fn assemble_classpath(
    project_root: Option<&Path>,
    explicit_class_paths: &[String],
    include_test_outputs: bool,
    additional_class_paths: &[String],
    jdk_home_override: Option<&Path>,
) -> Result<ClasspathPlan, ClasspathError> {
    let mut classpath: Vec<PathBuf> = additional_class_paths.iter().map(PathBuf::from).collect();
    if !explicit_class_paths.is_empty() {
        classpath.extend(explicit_class_paths.iter().map(PathBuf::from));
        let source_roots = project_root.map(discover_source_roots).unwrap_or_default();
        return Ok(ClasspathPlan {
            classpath,
            source_roots,
            scratch_dir: None,
        });
    }

    let Some(root) = project_root else {
        return Err(ClasspathError::new(
            "No classPaths given and no projectRoot to derive them from. \
             Set \"classPaths\" in launch.json or open a project folder.",
        ));
    };

    // Static, offline dependency resolution — never executes build scripts.
    let cp = jvl_classpath::Classpath::from_jdk_and_project(Some(root));
    let mut source_roots: Vec<PathBuf> = cp.source_roots().to_vec();
    for discovered in discover_source_roots(root) {
        if !source_roots.contains(&discovered) {
            source_roots.push(discovered);
        }
    }

    let existing_outputs: Vec<PathBuf> = jvl_classpath::module_output_dirs(root)
        .into_iter()
        .filter(|dir| dir.is_dir())
        .collect();
    let existing_test_outputs: Vec<PathBuf> = if include_test_outputs {
        jvl_classpath::module_test_output_dirs(root)
            .into_iter()
            .filter(|dir| dir.is_dir())
            .collect()
    } else {
        Vec::new()
    };
    // A test run without any built test classes must NOT use the derived
    // outputs (the tests wouldn't be on the classpath) — fall through to
    // the auto-compile fallback, which compiles `src/test/java` too.
    let derived_usable = !existing_outputs.is_empty()
        && (!include_test_outputs || !existing_test_outputs.is_empty());
    if derived_usable {
        classpath.extend(existing_outputs);
        classpath.extend(existing_test_outputs);
        classpath.extend(cp.entries().iter().cloned());
        return Ok(ClasspathPlan {
            classpath,
            source_roots,
            scratch_dir: None,
        });
    }

    // Auto-compile fallback.
    let Some(javac) = locate_javac(jdk_home_override) else {
        return Err(ClasspathError::new(
            "Project is not built and no JDK was found for the auto-compile \
             fallback. Set java-vsix-lite.jdk.home.",
        ));
    };
    let sources = collect_source_files(&source_roots);
    if sources.is_empty() {
        return Err(ClasspathError::new(format!(
            "No Java sources found under {} (looked in src/main/java, src/test/java). \
             Build the project with Maven/Gradle first or set \"classPaths\" in launch.json.",
            root.display()
        )));
    }
    let scratch = create_fresh_scratch_dir().ok_or_else(|| {
        ClasspathError::new("Could not create a temporary directory for the auto-compile output.")
    })?;
    match run_javac(&javac, &sources, cp.entries(), &scratch) {
        Ok(()) => {
            classpath.push(scratch.clone());
            classpath.extend(cp.entries().iter().cloned());
            Ok(ClasspathPlan {
                classpath,
                source_roots,
                scratch_dir: Some(scratch),
            })
        }
        Err(stderr) => {
            std::fs::remove_dir_all(&scratch).ok();
            Err(ClasspathError {
                message: "Build failed. Fix compile errors or build the project with \
                          Maven/Gradle first."
                    .to_string(),
                javac_stderr: Some(stderr),
            })
        }
    }
}

/// Run `javac -g -proc:none -encoding UTF-8 [-cp jars] -d scratch @argfile`,
/// argv-only, with poll/kill/reap timeout handling. `Err` carries the
/// captured (capped) stderr, or a description of the failure.
fn run_javac(
    javac: &Path,
    sources: &[PathBuf],
    jars: &[PathBuf],
    scratch: &Path,
) -> Result<(), String> {
    use std::process::{Command, Stdio};

    let argfile = scratch.join("sources.txt");
    if let Err(e) = write_argfile(&argfile, sources) {
        return Err(format!("could not write javac argfile: {e}"));
    }

    let mut command = Command::new(javac);
    // `-proc:none` is MANDATORY: annotation processors are arbitrary code
    // execution — same invariant as the server's javac check command.
    command
        .arg("-g")
        .arg("-proc:none")
        .arg("-encoding")
        .arg("UTF-8");
    if !jars.is_empty() {
        match std::env::join_paths(jars) {
            Ok(joined) => {
                command.arg("-cp").arg(joined);
            }
            Err(e) => return Err(format!("unrepresentable classpath entry: {e}")),
        }
    }
    command
        .arg("-d")
        .arg(scratch)
        .arg(format!("@{}", argfile.display()))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => return Err(format!("could not run javac: {e}")),
    };
    let stderr_pipe = child.stderr.take();
    let reader = std::thread::spawn(move || {
        let mut captured = Vec::new();
        if let Some(mut pipe) = stderr_pipe {
            use std::io::Read;
            let mut buf = [0u8; 8192];
            loop {
                match pipe.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        // Cap the kept bytes; keep draining so the child's
                        // pipe never fills up and blocks it.
                        let room = STDERR_CAP.saturating_sub(captured.len());
                        captured.extend_from_slice(&buf[..n.min(room)]);
                    }
                }
            }
        }
        captured
    });

    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {
                if start.elapsed() >= COMPILE_TIMEOUT {
                    child.kill().ok();
                    child.wait().ok(); // reap — no zombie
                    break None;
                }
                std::thread::sleep(POLL_INTERVAL);
            }
            Err(_) => break None,
        }
    };
    let stderr_bytes = reader.join().unwrap_or_default();
    let stderr = String::from_utf8_lossy(&stderr_bytes).into_owned();
    match status {
        Some(status) if status.success() => Ok(()),
        Some(_) => Err(stderr),
        None => Err(format!(
            "javac timed out after {}s\n{stderr}",
            COMPILE_TIMEOUT.as_secs()
        )),
    }
}

/// Create the scratch dir with `create_dir` (NOT `create_dir_all`): on a
/// shared world-writable temp dir a predictable pre-existing name (or a
/// pre-planted symlink) would let another local user redirect the class
/// output — `create_dir` fails on anything pre-existing, so each attempt is
/// guaranteed fresh. Same rationale as the server's javac scratch dir.
fn create_fresh_scratch_dir() -> Option<PathBuf> {
    for attempt in 0u32..8 {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "jvl-debug-{}-{}-{}",
            std::process::id(),
            nanos,
            attempt
        ));
        if std::fs::create_dir(&dir).is_ok() {
            return Some(dir);
        }
    }
    None
}

/// One (double-quoted, backslash/quote-escaped) path per line — a `javac`
/// `@argfile`, keeping large projects under `ARG_MAX`.
fn write_argfile(path: &Path, files: &[PathBuf]) -> std::io::Result<()> {
    let mut content = String::new();
    for file in files {
        let escaped = file
            .display()
            .to_string()
            .replace('\\', "\\\\")
            .replace('"', "\\\"");
        content.push('"');
        content.push_str(&escaped);
        content.push_str("\"\n");
    }
    std::fs::write(path, content)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "jvl-debug-launch-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn explicit_class_paths_win_without_derivation() {
        let plan =
            assemble_classpath(None, &["/tmp/classes".to_string()], false, &[], None).unwrap();
        assert_eq!(plan.classpath, vec![PathBuf::from("/tmp/classes")]);
        assert!(plan.scratch_dir.is_none());
    }

    #[test]
    fn additional_class_paths_precede_explicit_entries() {
        let plan = assemble_classpath(
            None,
            &["/tmp/classes".to_string()],
            false,
            &["/tmp/launcher.jar".to_string()],
            None,
        )
        .unwrap();
        assert_eq!(
            plan.classpath,
            vec![
                PathBuf::from("/tmp/launcher.jar"),
                PathBuf::from("/tmp/classes")
            ]
        );
    }

    #[test]
    fn derived_mode_adds_test_outputs_with_additional_entries_first() {
        let root = temp_root("test-outputs");
        std::fs::write(root.join("pom.xml"), "<project/>").unwrap();
        std::fs::create_dir_all(root.join("target/classes")).unwrap();
        std::fs::create_dir_all(root.join("target/test-classes")).unwrap();

        let plan = assemble_classpath(
            Some(&root),
            &[],
            true,
            &["/tmp/launcher.jar".to_string()],
            None,
        )
        .unwrap();
        assert!(plan.scratch_dir.is_none());
        assert!(
            plan.classpath.contains(&root.join("target/classes")),
            "{:?}",
            plan.classpath
        );
        assert!(
            plan.classpath.contains(&root.join("target/test-classes")),
            "{:?}",
            plan.classpath
        );
        // Additional entries come FIRST: the self-contained launcher jar
        // must shadow any project-resolved JUnit jars, or mixed JUnit
        // versions fail with NoSuchMethodError before any test runs.
        assert_eq!(
            plan.classpath.first(),
            Some(&PathBuf::from("/tmp/launcher.jar"))
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn derived_mode_without_test_outputs_is_ignored_for_plain_runs() {
        let root = temp_root("no-test-outputs-plain");
        std::fs::write(root.join("pom.xml"), "<project/>").unwrap();
        std::fs::create_dir_all(root.join("target/classes")).unwrap();

        // Without includeTestOutputs, missing test dirs don't matter.
        let plan = assemble_classpath(Some(&root), &[], false, &[], None).unwrap();
        assert!(plan.scratch_dir.is_none());
        assert!(
            plan.classpath.contains(&root.join("target/classes")),
            "{:?}",
            plan.classpath
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn missing_test_outputs_force_the_auto_compile_fallback() {
        let root = temp_root("no-test-outputs");
        std::fs::write(root.join("pom.xml"), "<project/>").unwrap();
        std::fs::create_dir_all(root.join("target/classes")).unwrap();

        // includeTestOutputs with no built test classes must NOT return the
        // derived-outputs plan; with no sources (and possibly no JDK) the
        // fallback errors — proving the derived path was rejected.
        let result = assemble_classpath(Some(&root), &[], true, &[], None);
        assert!(result.is_err(), "derived outputs must be rejected");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn discover_source_roots_finds_module_roots() {
        let root = temp_root("roots");
        std::fs::create_dir_all(root.join("src/main/java")).unwrap();
        std::fs::create_dir_all(root.join("modA/src/test/java")).unwrap();
        std::fs::write(root.join("modA/pom.xml"), "<project/>").unwrap();
        // No build file at `plain/` and it's not the root: not a module.
        std::fs::create_dir_all(root.join("plain/src/main/java")).unwrap();

        let roots = discover_source_roots(&root);
        assert!(roots.contains(&root.join("src/main/java")), "{roots:?}");
        assert!(
            roots.contains(&root.join("modA/src/test/java")),
            "{roots:?}"
        );
        assert!(
            !roots.contains(&root.join("plain/src/main/java")),
            "{roots:?}"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn locate_java_override_must_itself_be_valid() {
        let root = temp_root("locate");
        // Override set but no bin/java there: must be None, never a
        // fallback to JAVA_HOME (misconfiguration surfaces).
        assert!(locate_java(Some(&root)).is_none());
        std::fs::remove_dir_all(&root).ok();
    }
}
