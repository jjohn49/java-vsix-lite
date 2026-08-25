// Thin extension shell for java-vsix-lite.
//
// Per the implementation plan's "Process topology", this shell does NOT contain
// analysis logic. Its entire job is: resolve and launch the single Rust LSP
// server over stdio, surface its state in the status bar, and wire a couple of
// commands. All parsing/lint/IntelliSense (and, later, supervision of the
// optional javac tier) lives inside the Rust server.

import { execFile, spawn } from "child_process";
import * as fs from "fs";
import * as os from "os";
import * as path from "path";
import * as util from "util";
import * as vscode from "vscode";
import {
  ExecuteCommandRequest,
  LanguageClient,
  LanguageClientOptions,
  ServerOptions,
  State,
  TransportKind,
} from "vscode-languageclient/node";

import * as mavenFetch from "./mavenFetch";

// M5.4: the one-shot javac check command. The result shape mirrors the
// server's `checkProject` executeCommand response (see `crates/server/src/
// main.rs`'s `run_check_project` and `javac.rs`'s module doc comment for the
// security invariants — `-proc:none` mandatory, JDK discovered never
// downloaded, explicit invocation only).
interface CheckProjectResult {
  status: string;
  message?: string;
  errorCount?: number;
  warningCount?: number;
}

// The user-facing command (contributed in package.json, trust-gated below).
const CHECK_PROJECT_COMMAND = "java-vsix-lite.checkProject";
// The server-internal executeCommand id it forwards to. Deliberately NOT the
// same id: vscode-languageclient auto-registers a VS Code command for every
// id the server advertises in `executeCommandProvider`, and a duplicate of a
// command this extension registers itself would throw
// `command '<id>' already exists` during client startup.
const SERVER_CHECK_PROJECT_COMMAND = "jvl.checkProject.run";

// M6.2: the consent-gated dependency download command. Same collision-
// avoidance pattern as `CHECK_PROJECT_COMMAND`/`SERVER_CHECK_PROJECT_COMMAND`
// above — this extension-contributed id and the server's internal
// executeCommand id it drives (`SERVER_REBUILD_CLASSPATH_COMMAND`) must never
// be the same string.
const DOWNLOAD_DEPENDENCIES_COMMAND = "java-vsix-lite.downloadDependencies";
const INSTALL_DEPENDENCIES_COMMAND = "java-vsix-lite.installDependencies";
const SERVER_REBUILD_CLASSPATH_COMMAND = "jvl.classpath.rebuild";

// User-facing "refresh" command: re-reads the build files and local caches
// (`~/.m2`/`~/.gradle`) and republishes diagnostics WITHOUT restarting the
// server process — the light counterpart to `restartServer`. Drives the same
// server-internal `SERVER_REBUILD_CLASSPATH_COMMAND` the post-install loop
// uses. Purely offline (no network, no build-script execution), so unlike the
// download/install commands it is not trust-gated.
const REBUILD_CLASSPATH_COMMAND = "java-vsix-lite.rebuildClasspath";

// Hard ceiling on a build-tool run before it's killed (dependency resolution
// can legitimately take minutes on a cold cache; a hung/interactive process
// must not block forever).
const INSTALL_TIMEOUT_MS = 15 * 60 * 1000;

// Fixed-point loop bounds (see the task brief): a runaway or maliciously deep
// transitive graph must never turn one consented download into an unbounded
// one.
const MAX_DOWNLOAD_ROUNDS = 5;
const MAX_ARTIFACTS_PER_INVOCATION = 300;
const MAX_TOTAL_BYTES_PER_INVOCATION = 200 * 1024 * 1024;
/** How many coordinates the consent dialog / skipped summary lists by name before collapsing into "and N more". */
const CONSENT_DISPLAY_CAP = 20;

/** One fetchable `g:a:v` from the server's `jvl/missingDependencies` response. */
interface ServerCoordinate {
  group: string;
  artifact: string;
  version: string;
}

/** One degraded coordinate the server won't offer to download, with why. */
interface ServerSkippedDependency {
  group: string;
  artifact: string;
  version?: string;
  reason: string;
}

interface MissingDependenciesResult {
  missing: ServerCoordinate[];
  skipped: ServerSkippedDependency[];
}

function coordLabel(coord: ServerCoordinate): string {
  return `${coord.group}:${coord.artifact}:${coord.version}`;
}

let client: LanguageClient | undefined;
let statusBar: vscode.StatusBarItem;

// Read-only virtual documents for external (JDK/dependency) goto-definition
// targets: the server resolves these to `jvl-src:/<fqn>.java` `Location`s;
// this provider fetches their content on demand via the `jvl/externalSource`
// custom request (real source when available, else a signature-only stub —
// the server decides which).
class ExternalSourceProvider implements vscode.TextDocumentContentProvider {
  async provideTextDocumentContent(uri: vscode.Uri): Promise<string> {
    if (!client) {
      return "";
    }
    const result = await client.sendRequest<{ text: string }>("jvl/externalSource", {
      uri: uri.toString(),
    });
    return result.text;
  }
}

export async function activate(context: vscode.ExtensionContext): Promise<void> {
  statusBar = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Left, 0);
  statusBar.text = "$(loading~spin) Java Lite";
  statusBar.tooltip = "java-vsix-lite language server";
  statusBar.show();
  context.subscriptions.push(statusBar);

  context.subscriptions.push(
    vscode.commands.registerCommand("java-vsix-lite.restartServer", async () => {
      await restart(context);
    }),
  );

  context.subscriptions.push(
    vscode.commands.registerCommand(CHECK_PROJECT_COMMAND, async () => {
      await checkProject();
    }),
  );

  context.subscriptions.push(
    vscode.commands.registerCommand(REBUILD_CLASSPATH_COMMAND, async () => {
      await rebuildClasspath();
    }),
  );

  context.subscriptions.push(
    vscode.commands.registerCommand(DOWNLOAD_DEPENDENCIES_COMMAND, async () => {
      await downloadDependencies();
    }),
  );

  context.subscriptions.push(
    vscode.commands.registerCommand(INSTALL_DEPENDENCIES_COMMAND, async () => {
      await installDependencies();
    }),
  );

  context.subscriptions.push(
    vscode.workspace.registerTextDocumentContentProvider("jvl-src", new ExternalSourceProvider()),
  );

  // M8b: real compiler errors on save — a debounced, silent `checkProject`
  // run after every Java file save, in trusted workspaces only (the same
  // trust gate as the manual command; `javac` is a spawned process). Silent
  // means silent: results reach Problems via the server's published
  // diagnostics, never a pop-up.
  context.subscriptions.push(
    vscode.workspace.onDidSaveTextDocument((doc) => {
      if (
        doc.languageId === "java" &&
        doc.uri.scheme === "file" &&
        javacBackgroundCheckEnabled()
      ) {
        scheduleSaveCheck(doc.uri.toString());
      }
    }),
  );

  // M8b follow-up: granting trust mid-session unlocks the background check
  // — run the on-load pass then, since activation skipped it.
  context.subscriptions.push(
    vscode.workspace.onDidGrantWorkspaceTrust(() => {
      if (javacBackgroundCheckEnabled()) {
        scheduleOpenDocsCheck();
      }
    }),
  );

  // VS Code normally infers the `java` language id from the `.java` extension
  // in a jvl-src URI's path, but that inference is what the documentSelector
  // match (and thus server sync) hinges on — pin it explicitly so the virtual
  // docs always reach the server regardless of detection quirks.
  context.subscriptions.push(
    vscode.workspace.onDidOpenTextDocument((doc) => {
      if (doc.uri.scheme === "jvl-src" && doc.languageId !== "java") {
        void vscode.languages.setTextDocumentLanguage(doc, "java");
      }
    }),
  );

  // Debugging: the DAP adapter is the same machine-scoped `jvl-server`
  // binary (env override → machine setting → bundled) run with the `dap`
  // subcommand, so a workspace can never redirect which binary debugs it.
  // The configuration provider enforces the Workspace Trust gate — the
  // debugger runs project code, so this refusal is load-bearing, exactly
  // like `checkProject()`'s.
  context.subscriptions.push(
    vscode.debug.registerDebugAdapterDescriptorFactory("java-vsix-lite", {
      createDebugAdapterDescriptor(): vscode.DebugAdapterDescriptor | undefined {
        const serverPath = resolveServerPath(context);
        if (!serverPath) {
          void vscode.window.showErrorMessage(
            "java-vsix-lite: could not locate the jvl-server binary for debugging.",
          );
          return undefined;
        }
        return new vscode.DebugAdapterExecutable(serverPath, ["dap"]);
      },
    }),
  );
  context.subscriptions.push(
    vscode.debug.registerDebugConfigurationProvider(
      "java-vsix-lite",
      new JavaDebugConfigurationProvider(),
    ),
  );

  await start(context);
}

export async function deactivate(): Promise<void> {
  await client?.stop();
  client = undefined;
}

function resolveServerPath(context: vscode.ExtensionContext): string | undefined {
  const fromEnv = process.env.JVL_SERVER_PATH;
  if (fromEnv && fromEnv.length > 0) {
    return fromEnv;
  }
  const configured = vscode.workspace
    .getConfiguration("java-vsix-lite")
    .get<string>("server.path");
  if (configured && configured.length > 0) {
    return configured;
  }
  const binary = process.platform === "win32" ? "jvl-server.exe" : "jvl-server";
  const bundled = context.asAbsolutePath(path.join("server", binary));
  return fs.existsSync(bundled) ? bundled : undefined;
}

// The fully-qualified main class of the active Java editor, when it visibly
// declares a `public static void main` — powers F5-with-no-launch.json and
// the generated launch.json template.
function detectMainClass(): string | undefined {
  const editor = vscode.window.activeTextEditor;
  if (!editor || editor.document.languageId !== "java") {
    return undefined;
  }
  const text = editor.document.getText();
  if (!/public\s+static\s+void\s+main\s*\(/.test(text)) {
    return undefined;
  }
  const pkg = /^\s*package\s+([\w.]+)\s*;/m.exec(text)?.[1];
  const stem = path.basename(editor.document.uri.fsPath).replace(/\.java$/, "");
  return pkg ? `${pkg}.${stem}` : stem;
}

class JavaDebugConfigurationProvider implements vscode.DebugConfigurationProvider {
  provideDebugConfigurations(): vscode.DebugConfiguration[] {
    return [
      {
        type: "java-vsix-lite",
        request: "launch",
        name: "Launch Java program",
        mainClass: detectMainClass() ?? "",
      },
    ];
  }

  resolveDebugConfiguration(
    folder: vscode.WorkspaceFolder | undefined,
    config: vscode.DebugConfiguration,
  ): vscode.DebugConfiguration | undefined {
    // Trust gate FIRST: debugging launches (or attaches to) project code.
    // Returning undefined aborts the session before any process spawns.
    if (!vscode.workspace.isTrusted) {
      void vscode.window.showErrorMessage(
        "java-vsix-lite: debugging is disabled in untrusted workspaces because it runs project code. Trust this workspace to enable it.",
      );
      return undefined;
    }

    // F5 with no launch.json: synthesize a launch config from the active
    // Java editor's main class.
    if (!config.type && !config.request && !config.name) {
      const mainClass = detectMainClass();
      if (!mainClass) {
        void vscode.window.showErrorMessage(
          "java-vsix-lite: Open the Java file containing the main method, or create a launch.json.",
        );
        return undefined;
      }
      config = {
        type: "java-vsix-lite",
        request: "launch",
        name: "Launch Java program",
        mainClass,
      };
    }

    // Unconditionally inject the machine-scoped JDK home, overwriting
    // anything workspace-provided — a workspace launch.json must never be
    // able to redirect which JVM binary runs (same rationale as the
    // machine-scoped path settings). Undefined is fine: the adapter falls
    // back to $JAVA_HOME, then filesystem JDK discovery.
    const jdkHome = vscode.workspace.getConfiguration("java-vsix-lite").get<string>("jdk.home");
    config.__jvlJdkHome = jdkHome && jdkHome.length > 0 ? jdkHome : undefined;

    // Default the project root (and launch cwd) to the workspace folder.
    const folderPath = folder?.uri.fsPath;
    if (folderPath) {
      if (!config.projectRoot) {
        config.projectRoot = folderPath;
      }
      if (config.request === "launch" && !config.cwd) {
        config.cwd = folderPath;
      }
    }
    return config;
  }
}

const execFileAsync = util.promisify(execFile);

// Version handshake: runs the server binary with `--version` and warns (via
// the LSP output channel, non-fatally) if it disagrees with the extension's
// own version. Catches a stale bundled binary left over from a partial
// update; never blocks startup — a spawn failure or an older binary that
// doesn't understand `--version` is swallowed as a warning too.
//
// Runs asynchronously and is fire-and-forget from the caller's perspective:
// a hung or misbehaving binary must not stall activation (the previous
// execFileSync-based implementation could block the entire shared extension
// host for up to its 5s timeout).
async function checkVersionHandshake(
  serverPath: string,
  extensionVersion: string,
  outputChannel: vscode.OutputChannel,
): Promise<void> {
  try {
    const { stdout } = await execFileAsync(serverPath, ["--version"], {
      encoding: "utf8",
      timeout: 5000,
    });
    const serverVersion = stdout.trim();
    if (serverVersion.length > 0 && serverVersion !== extensionVersion) {
      outputChannel.appendLine(
        `[java-vsix-lite] warning: bundled server version (${serverVersion}) does not match extension version (${extensionVersion}); consider reinstalling the extension.`,
      );
    }
  } catch (err) {
    outputChannel.appendLine(
      `[java-vsix-lite] warning: could not verify jvl-server's version (${String(err)}).`,
    );
  }
}

async function start(context: vscode.ExtensionContext): Promise<void> {
  const serverPath = resolveServerPath(context);
  if (!serverPath) {
    statusBar.text = "$(error) Java Lite";
    statusBar.tooltip = "jvl-server binary not found";
    void vscode.window.showErrorMessage(
      "java-vsix-lite: could not locate the `jvl-server` binary. Set `java-vsix-lite.server.path` or the JVL_SERVER_PATH environment variable.",
    );
    return;
  }

  const serverOptions: ServerOptions = {
    run: { command: serverPath, transport: TransportKind.stdio },
    debug: {
      command: serverPath,
      transport: TransportKind.stdio,
      options: { env: { ...process.env, JVL_LOG: "jvl_server=debug,warn" } },
    },
  };

  const clientOptions: LanguageClientOptions = {
    // `jvl-src` is included so the virtual documents served by
    // ExternalSourceProvider are synced to the server too — hover, further
    // go-to-definition, and semantic tokens keep working while browsing
    // external/JDK source.
    documentSelector: [
      { scheme: "file", language: "java" },
      { scheme: "jvl-src", language: "java" },
    ],
    outputChannelName: "java-vsix-lite",
    initializationOptions: {
      unresolvedMemberDiagnostics: vscode.workspace
        .getConfiguration("java-vsix-lite")
        .get<boolean>("diagnostics.unresolvedMembers", true),
      // M5.4: an explicit override for where to find `javac`, tried before
      // $JAVA_HOME (empty string means "unset" — the server falls back).
      jdkHome: vscode.workspace.getConfiguration("java-vsix-lite").get<string>("jdk.home", ""),
      javacTimeoutSecs: vscode.workspace
        .getConfiguration("java-vsix-lite")
        .get<number>("javac.timeoutSecs", 120),
    },
    middleware: {
      // M7 (fixed): the first `publishDiagnostics` after startup signals
      // that the classpath has been built at least once — the proactive
      // dependency check's trigger. This MUST be middleware, never
      // `client.onNotification("textDocument/publishDiagnostics", …)`:
      // the underlying jsonrpc connection keeps ONE handler per method, so
      // a user-registered handler *replaces* the client's built-in
      // diagnostics handling and silently kills every squiggle, Problems
      // entry, and error file-name decoration (field-reported).
      handleDiagnostics: (uri, diagnostics, next) => {
        next(uri, diagnostics);
        if (!proactiveTriggerFired) {
          proactiveTriggerFired = true;
          if (client) {
            void maybeProactiveDependencyCheck(client);
          }
        }
      },
    },
  };

  client = new LanguageClient(
    "java-vsix-lite",
    "java-vsix-lite",
    serverOptions,
    clientOptions,
  );

  // Fire-and-forget: activation must not wait on this (see
  // checkVersionHandshake's doc comment).
  void checkVersionHandshake(
    serverPath,
    context.extension.packageJSON.version as string,
    client.outputChannel,
  );

  client.onDidChangeState((event) => updateStatus(event.newState));
  context.subscriptions.push(client);

  await client.start();

  // M8b follow-up: one silent check when the project loads (and again after
  // a server restart), so pre-existing errors surface without waiting for
  // the first save. Now scoped to the modules of whatever Java documents are
  // already open — activation never eagerly compiles the whole workspace. Same
  // gate, debounce, and single-flight as the on-save path.
  if (javacBackgroundCheckEnabled()) {
    scheduleOpenDocsCheck();
  }
}

// Shared gate for the background (save/load-triggered) javac checks:
// trusted workspace + the `javac.checkOnSave` setting + a real workspace
// folder to collect sources from.
function javacBackgroundCheckEnabled(): boolean {
  return (
    vscode.workspace.isTrusted &&
    (vscode.workspace.workspaceFolders?.length ?? 0) > 0 &&
    vscode.workspace
      .getConfiguration("java-vsix-lite")
      .get<boolean>("javac.checkOnSave", true)
  );
}

async function restart(context: vscode.ExtensionContext): Promise<void> {
  await client?.stop();
  client = undefined;
  await start(context);
}

// The light "refresh": ask the running server to re-read the build files and
// local dependency caches and rebuild the classpath, then republish
// diagnostics — without tearing down the process (that's `restartServer`).
// Use it after editing a `pom.xml`/`build.gradle` the watcher didn't catch, or
// after dropping a jar into `~/.m2` by hand. Offline and side-effect-free
// (no network, no build-script execution), so no trust gate.
async function rebuildClasspath(): Promise<void> {
  if (!client) {
    void vscode.window.showErrorMessage("java-vsix-lite: the language server is not running.");
    return;
  }

  const previousText = statusBar.text;
  const previousTooltip = statusBar.tooltip;
  statusBar.text = "$(loading~spin) Java Lite";
  statusBar.tooltip = "java-vsix-lite: rebuilding classpath…";
  try {
    await client.sendRequest(ExecuteCommandRequest.type, {
      command: SERVER_REBUILD_CLASSPATH_COMMAND,
      arguments: [],
    });
    void vscode.window.showInformationMessage("java-vsix-lite: classpath rebuilt.");
    // Re-run the silent javac check so Problems reflects the refreshed
    // classpath too (same gate/debounce as save; no-op in untrusted workspaces).
    if (javacBackgroundCheckEnabled()) {
      scheduleOpenDocsCheck();
    }
  } catch (err) {
    void vscode.window.showErrorMessage(
      `java-vsix-lite: could not rebuild the classpath: ${String(err)}`,
    );
  } finally {
    statusBar.text = previousText;
    statusBar.tooltip = previousTooltip;
  }
}

// M5.4: the one-shot, trust-gated javac check command. Spawning javac is
// build-adjacent (it compiles project code), so — per the threat model —
// this refuses outright in an untrusted workspace, same as the (still
// unimplemented) Gradle/Maven build commands. This is the *only* place that
// gate is enforced on the extension side; the server has no notion of
// Workspace Trust and just does what it's told, so this check is load-
// bearing, not decorative.
async function checkProject(): Promise<void> {
  if (!vscode.workspace.isTrusted) {
    void vscode.window.showErrorMessage(
      "java-vsix-lite: Check Project (javac) is disabled in an untrusted workspace — it spawns the JDK's javac compiler. Trust this workspace to enable it.",
    );
    return;
  }
  if (!client) {
    void vscode.window.showErrorMessage("java-vsix-lite: the language server is not running.");
    return;
  }

  const previousText = statusBar.text;
  const previousTooltip = statusBar.tooltip;
  statusBar.text = "$(loading~spin) Java Lite";
  statusBar.tooltip = "java-vsix-lite: checking project (javac)…";
  try {
    const result = await client.sendRequest(ExecuteCommandRequest.type, {
      command: SERVER_CHECK_PROJECT_COMMAND,
      // The manual command is always a full-workspace check.
      arguments: [{ scope: "project" }],
    });
    reportCheckProjectResult(result as CheckProjectResult);
  } catch (err) {
    void vscode.window.showErrorMessage(`java-vsix-lite: Check Project failed: ${String(err)}`);
  } finally {
    statusBar.text = previousText;
    statusBar.tooltip = previousTooltip;
  }
}

function reportCheckProjectResult(result: CheckProjectResult): void {
  switch (result.status) {
    case "ok": {
      const errors = result.errorCount ?? 0;
      const warnings = result.warningCount ?? 0;
      if (errors > 0) {
        void vscode.window.showErrorMessage(
          `java-vsix-lite: Check Project found ${errors} error(s), ${warnings} warning(s). See Problems.`,
        );
      } else {
        void vscode.window.showInformationMessage(
          `java-vsix-lite: Check Project passed (${warnings} warning(s)).`,
        );
      }
      break;
    }
    case "already-running":
      void vscode.window.showInformationMessage(
        "java-vsix-lite: Check Project is already running.",
      );
      break;
    case "javac-not-found":
      void vscode.window.showErrorMessage(
        `java-vsix-lite: could not locate javac (${result.message ?? "not found"}). Set $JAVA_HOME or the java-vsix-lite.jdk.home setting.`,
      );
      break;
    case "timeout":
      void vscode.window.showErrorMessage(
        `java-vsix-lite: Check Project timed out. ${result.message ?? ""}`,
      );
      break;
    case "jdk-too-old":
      notifyJdkTooOld(result);
      break;
    default:
      void vscode.window.showErrorMessage(
        `java-vsix-lite: Check Project failed: ${result.message ?? result.status}`,
      );
  }
}

// The detected JDK is older than the project's declared Java level, so the
// javac check was skipped (the server already published a single diagnostic on
// the build file). Surface it as a notification too, with a shortcut to the
// machine-scoped JDK override.
function notifyJdkTooOld(result: CheckProjectResult): void {
  const detail =
    result.message ?? "the detected JDK is too old for this project's Java level";
  void vscode.window
    .showWarningMessage(
      `java-vsix-lite: ${detail}. Install a newer JDK, or set java-vsix-lite.jdk.home to one.`,
      "Configure JDK path",
    )
    .then((choice) => {
      if (choice === "Configure JDK path") {
        void vscode.commands.executeCommand(
          "workbench.action.openSettings",
          "java-vsix-lite.jdk.home",
        );
      }
    });
}

// M8b: check-on-save plumbing, now MODULE-SCOPED. Each background check
// compiles only the Maven/Gradle modules owning the files saved in the debounce
// window — not the whole workspace (the manual `Check Project (javac)` command
// stays project-wide). `pendingSaveCheckUris` accumulates the file-backed Java
// document URIs to check next; the debounce coalesces a burst of saves ("Save
// All") into one run, and any save landing mid-run stays in the set for exactly
// one follow-up run with the newest set (the server answers `already-running`
// to a concurrent request, so at most one javac ever runs).
const pendingSaveCheckUris = new Set<string>();
let saveCheckTimer: ReturnType<typeof setTimeout> | undefined;
let saveCheckRunning = false;
const SAVE_CHECK_DEBOUNCE_MS = 1500;
// The background check is silent, but the JDK-too-old *configuration* problem
// is surfaced once (not on every save). Reset when a check no longer reports
// it, so fixing then re-breaking the JDK notifies again.
let jdkTooOldNotified = false;

/** (Re)arm the shared debounce timer without touching the pending set. */
function armSaveCheckTimer(): void {
  if (saveCheckTimer !== undefined) {
    clearTimeout(saveCheckTimer);
  }
  saveCheckTimer = setTimeout(() => {
    saveCheckTimer = undefined;
    void runSaveCheck();
  }, SAVE_CHECK_DEBOUNCE_MS);
}

/** Queue one saved Java document for the next scoped background check. */
function scheduleSaveCheck(uri: string): void {
  pendingSaveCheckUris.add(uri);
  armSaveCheckTimer();
}

/**
 * Queue every currently-open, file-backed Java document for a scoped check —
 * the activation / restart / trust-grant / post-install entry point (there is
 * no single "saved file" to key off in those cases). Deliberately does NOT
 * fall back to a whole-project compile: if no Java documents are open, there is
 * nothing to check yet, so it schedules nothing.
 */
function scheduleOpenDocsCheck(): void {
  let queuedAny = false;
  for (const doc of vscode.workspace.textDocuments) {
    if (doc.languageId === "java" && doc.uri.scheme === "file") {
      pendingSaveCheckUris.add(doc.uri.toString());
      queuedAny = true;
    }
  }
  if (queuedAny) {
    armSaveCheckTimer();
  }
}

async function runSaveCheck(): Promise<void> {
  if (saveCheckRunning) {
    // A check is in flight; the pending set is left intact so the follow-up
    // scheduled in `finally` picks these saves up.
    return;
  }
  // Re-checked here (not just at schedule time): trust or the running client
  // can be gone by the time the debounce fires.
  if (!client || !vscode.workspace.isTrusted) {
    pendingSaveCheckUris.clear();
    return;
  }
  if (pendingSaveCheckUris.size === 0) {
    return;
  }
  // Snapshot and clear: saves landing during the run re-populate the set for a
  // follow-up, rather than being lost or folded into this run's fixed input.
  const documentUris = [...pendingSaveCheckUris];
  pendingSaveCheckUris.clear();
  saveCheckRunning = true;
  try {
    const result = (await client.sendRequest(ExecuteCommandRequest.type, {
      command: SERVER_CHECK_PROJECT_COMMAND,
      arguments: [{ scope: "modules", documentUris }],
    })) as CheckProjectResult;
    // Otherwise silent, but the JDK-too-old state is a config problem worth a
    // one-time toast (the diagnostic on the build file is easy to miss).
    if (result.status === "jdk-too-old") {
      if (!jdkTooOldNotified) {
        jdkTooOldNotified = true;
        notifyJdkTooOld(result);
      }
    } else {
      jdkTooOldNotified = false;
    }
  } catch {
    // Silent by design — a failed background check must never toast on save.
    // The manual `Java: Check Project (javac)` command reports errors.
  } finally {
    saveCheckRunning = false;
    // A save landed during the run (or the timer fired mid-run): run once more
    // with whatever accumulated.
    if (pendingSaveCheckUris.size > 0) {
      armSaveCheckTimer();
    }
  }
}

// M6.2: one download invocation at a time — `true` while one is in flight
// (from the missing-deps query through the end of the download loop). The
// extension-side mirror of `checkProject`'s single-flight pattern (the server
// enforces that one via `javac_running`; downloads are driven entirely by
// this process, so the flag lives here). No queuing: a second invocation is
// simply told one is already running.
let downloadInFlight = false;

// ---------------------------------------------------------------------------
// Install dependencies by running the project's build tool (Maven/Gradle).
//
// SECURITY: unlike `Download Missing Dependencies` (direct HTTPS + checksum,
// no tool execution), this runs `mvn`/`gradle`, which EXECUTES the project's
// build scripts — Maven plugins run through the lifecycle; a `build.gradle` is
// a Groovy/Kotlin program. That is arbitrary code from the workspace, so it is
// gated exactly like the `javac` tier (trusted workspaces only) AND requires
// an explicit per-invocation confirmation. It is never run automatically. Its
// purpose is to populate the local cache (`~/.m2`, `~/.gradle`) with the
// BOM/parent-managed transitive versions the offline resolver can't determine
// on its own (the case where `Download Missing Dependencies` finds nothing to
// fetch because every needed coordinate has an unresolved version).
// ---------------------------------------------------------------------------

let installInFlight = false;

interface BuildTool {
  kind: "maven" | "gradle";
  /** Absolute path (wrapper) or bare command name to execute. */
  command: string;
  args: string[];
  cwd: string;
  /** True when `command` is the project-shipped wrapper (an extra note in the
   *  confirmation, since running it executes project-controlled code too). */
  isWrapper: boolean;
}

async function installDependencies(): Promise<void> {
  if (!vscode.workspace.isTrusted) {
    void vscode.window.showErrorMessage(
      "java-vsix-lite: Install Dependencies is disabled in an untrusted workspace — it runs your project's build tool, which executes the project's build scripts. Trust this workspace to enable it.",
    );
    return;
  }
  if (installInFlight) {
    void vscode.window.showInformationMessage(
      "java-vsix-lite: a dependency install is already running.",
    );
    return;
  }
  const folder = vscode.workspace.workspaceFolders?.[0];
  if (!folder || folder.uri.scheme !== "file") {
    void vscode.window.showErrorMessage(
      "java-vsix-lite: open a Maven or Gradle project folder first.",
    );
    return;
  }
  const tool = detectBuildTool(folder.uri.fsPath);
  if (!tool) {
    void vscode.window.showErrorMessage(
      "java-vsix-lite: no `pom.xml` or Gradle build file found at the workspace root.",
    );
    return;
  }

  const confirm = await vscode.window.showWarningMessage(
    `Run ${tool.kind === "maven" ? "Maven" : "Gradle"} to install dependencies?`,
    {
      modal: true,
      detail:
        `Runs \`${path.basename(tool.command)} ${tool.args.join(" ")}\` in ${tool.cwd}.\n\n` +
        `⚠ This EXECUTES this project's build scripts` +
        (tool.isWrapper
          ? ` (including the project-shipped ${tool.kind === "maven" ? "Maven" : "Gradle"} wrapper)`
          : "") +
        `, which can run arbitrary code. Only continue for a project you trust.`,
    },
    "Run",
  );
  if (confirm !== "Run") {
    return;
  }

  installInFlight = true;
  try {
    await runInstall(tool);
  } finally {
    installInFlight = false;
  }
}

/**
 * Locate the build tool for `root`: Maven when a `pom.xml` is present, else
 * Gradle when a Gradle build/settings file is. Prefers the project wrapper
 * (`mvnw`/`gradlew`), then a machine-configured path, then the tool on `PATH`.
 * `undefined` when neither project type applies.
 */
function detectBuildTool(root: string): BuildTool | undefined {
  const win = process.platform === "win32";
  const has = (name: string) => fs.existsSync(path.join(root, name));
  if (has("pom.xml")) {
    const wrapper = path.join(root, win ? "mvnw.cmd" : "mvnw");
    const { command, isWrapper } = resolveTool("maven", wrapper, win);
    return { kind: "maven", command, args: ["-B", "dependency:go-offline"], cwd: root, isWrapper };
  }
  if (
    has("build.gradle") ||
    has("build.gradle.kts") ||
    has("settings.gradle") ||
    has("settings.gradle.kts")
  ) {
    const wrapper = path.join(root, win ? "gradlew.bat" : "gradlew");
    const { command, isWrapper } = resolveTool("gradle", wrapper, win);
    return { kind: "gradle", command, args: ["--console=plain", "dependencies"], cwd: root, isWrapper };
  }
  return undefined;
}

/**
 * Pick the executable to run: the project wrapper if present, else a
 * machine-configured absolute path, else the tool discovered on `PATH` or in
 * common install locations, else the bare name (so a clean ENOENT still tells
 * the user what's missing). The common-location probe matters because a
 * GUI-launched editor often has a minimal `PATH` that omits Homebrew/SDKMAN —
 * the same reason `$JAVA_HOME` is empty there.
 */
function resolveTool(
  kind: "maven" | "gradle",
  wrapper: string,
  win: boolean,
): { command: string; isWrapper: boolean } {
  if (fs.existsSync(wrapper)) {
    return { command: wrapper, isWrapper: true };
  }
  const configured = configuredToolPath(kind);
  if (configured) {
    return { command: configured, isWrapper: false };
  }
  const bare = kind === "maven" ? (win ? "mvn.cmd" : "mvn") : win ? "gradle.bat" : "gradle";
  return { command: locateExecutable(bare, kind) ?? bare, isWrapper: false };
}

/** Search `$PATH` then common install dirs for `exe`; absolute path or `undefined`. */
function locateExecutable(exe: string, kind: "maven" | "gradle"): string | undefined {
  const pathDirs = (process.env.PATH ?? "").split(path.delimiter).filter(Boolean);
  const home = os.homedir();
  const common = [
    "/opt/homebrew/bin",
    "/usr/local/bin",
    "/usr/bin",
    path.join(home, ".sdkman", "candidates", kind === "maven" ? "maven" : "gradle", "current", "bin"),
    `/opt/${kind === "maven" ? "maven" : "gradle"}/bin`,
  ];
  for (const dir of [...pathDirs, ...common]) {
    const candidate = path.join(dir, exe);
    try {
      if (fs.existsSync(candidate) && fs.statSync(candidate).isFile()) {
        return candidate;
      }
    } catch {
      // unreadable dir — skip
    }
  }
  return undefined;
}

/** A machine-scoped override for the Maven/Gradle executable, or `undefined`. */
function configuredToolPath(kind: "maven" | "gradle"): string | undefined {
  const raw = vscode.workspace
    .getConfiguration("java-vsix-lite")
    .get<string>(`${kind}.path`, "")
    .trim();
  return raw.length > 0 ? raw : undefined;
}

async function runInstall(tool: BuildTool): Promise<void> {
  const channel = client?.outputChannel;
  if (channel) {
    channel.show(true);
    channel.appendLine(`\n$ ${tool.command} ${tool.args.join(" ")}   (cwd: ${tool.cwd})`);
  }

  const outcome = await vscode.window.withProgress(
    {
      location: vscode.ProgressLocation.Notification,
      title: `java-vsix-lite: installing ${tool.kind === "maven" ? "Maven" : "Gradle"} dependencies…`,
      cancellable: true,
    },
    (_progress, token) => runBuildTool(tool, channel, token),
  );

  if (outcome === "cancelled") {
    void vscode.window.showInformationMessage("java-vsix-lite: dependency install cancelled.");
  } else if (outcome === "spawn-error") {
    void vscode.window.showErrorMessage(
      `java-vsix-lite: could not run ${path.basename(tool.command)} — is it installed and on your PATH? ` +
        `Set java-vsix-lite.${tool.kind}.path, or add a wrapper (${tool.kind === "maven" ? "mvnw" : "gradlew"}) to the project.`,
    );
  } else if (outcome !== 0) {
    void vscode.window.showWarningMessage(
      `java-vsix-lite: ${path.basename(tool.command)} exited with code ${outcome}. ` +
        `Some dependencies may still have installed — see the java-vsix-lite output.`,
    );
  } else {
    void vscode.window.showInformationMessage(
      "java-vsix-lite: dependencies installed. Refreshing IntelliSense…",
    );
  }

  // Whatever resolved is now cached even on a nonzero/partial exit — rebuild
  // the classpath so IntelliSense picks it up, then re-run the javac check.
  if (client && outcome !== "cancelled" && outcome !== "spawn-error") {
    await client
      .sendRequest(ExecuteCommandRequest.type, {
        command: SERVER_REBUILD_CLASSPATH_COMMAND,
        arguments: [],
      })
      .catch(() => undefined);
    if (javacBackgroundCheckEnabled()) {
      scheduleOpenDocsCheck();
    }
  }
}

/**
 * Run `tool`, streaming stdout/stderr to `channel`, killed on cancellation or
 * the hard timeout. Resolves to the exit code, or `"cancelled"` /
 * `"spawn-error"`. Uses `spawn` (no shell — args are a fixed array, never
 * concatenated) so a crafted path can't inject extra commands.
 */
function runBuildTool(
  tool: BuildTool,
  channel: vscode.OutputChannel | undefined,
  token: vscode.CancellationToken,
): Promise<number | "cancelled" | "spawn-error"> {
  return new Promise((resolve) => {
    const child = spawn(tool.command, tool.args, {
      cwd: tool.cwd,
      shell: false,
      timeout: INSTALL_TIMEOUT_MS,
    });
    let settled = false;
    const finish = (result: number | "cancelled" | "spawn-error") => {
      if (!settled) {
        settled = true;
        resolve(result);
      }
    };
    child.stdout?.on("data", (d: Buffer) => channel?.append(d.toString()));
    child.stderr?.on("data", (d: Buffer) => channel?.append(d.toString()));
    child.on("error", (err) => {
      channel?.appendLine(`\n[spawn error] ${String(err)}`);
      finish("spawn-error");
    });
    child.on("close", (code) => finish(code ?? 1));
    token.onCancellationRequested(() => {
      channel?.appendLine("\n[cancelled by user]");
      child.kill("SIGTERM");
      finish("cancelled");
    });
  });
}

// M6.2: the consent-gated dependency download command. Trust-gated like
// `checkProject` (this one performs network I/O and writes into `~/.m2`,
// both squarely "acts on behalf of this project" territory), then:
// `jvl/missingDependencies` -> one modal consent dialog -> a bounded
// fixed-point download/rebuild loop. See `mavenFetch.ts` for the actual
// HTTPS/checksum/install logic and the threat-model notes on what checksum
// verification does and doesn't protect against.
async function downloadDependencies(): Promise<void> {
  if (!vscode.workspace.isTrusted) {
    void vscode.window.showErrorMessage(
      "java-vsix-lite: Download Missing Dependencies is disabled in an untrusted workspace — it downloads files over the network and installs them into ~/.m2. Trust this workspace to enable it.",
    );
    return;
  }
  if (!client) {
    void vscode.window.showErrorMessage("java-vsix-lite: the language server is not running.");
    return;
  }
  if (downloadInFlight) {
    void vscode.window.showInformationMessage(
      "java-vsix-lite: a dependency download is already running.",
    );
    return;
  }
  downloadInFlight = true;
  try {
    await runDownloadDependencies(client);
  } finally {
    downloadInFlight = false;
  }
}

/** The body of `downloadDependencies`, guarded single-flight by its caller. */
async function runDownloadDependencies(activeClient: LanguageClient): Promise<void> {
  // `activeClient` was captured once by the caller: `client` is mutable
  // module state (a restart could swap it out from under an in-flight,
  // possibly long-running, download loop).
  let initial: MissingDependenciesResult;
  try {
    initial = await activeClient.sendRequest<MissingDependenciesResult>(
      "jvl/missingDependencies",
    );
  } catch (err) {
    void vscode.window.showErrorMessage(
      `java-vsix-lite: could not query missing dependencies: ${String(err)}`,
    );
    return;
  }

  if (initial.missing.length === 0) {
    if (initial.skipped.length > 0) {
      // These couldn't be resolved to a downloadable `g:a:v` — most commonly
      // a version managed by a parent POM / BOM that isn't in the local cache
      // (so the offline resolver can't tell which version to fetch). Direct
      // download can't help here; running the build tool once can.
      const choice = await vscode.window.showInformationMessage(
        `java-vsix-lite: nothing can be downloaded directly — ` +
          `${initial.skipped.length} dependency(ies) have an unresolved version ` +
          `(usually a parent POM/BOM missing from your local cache): ${summarizeSkipped(initial.skipped)}. ` +
          `Run the build tool once to populate the cache.`,
        "Install Dependencies",
      );
      if (choice === "Install Dependencies") {
        await installDependencies();
      }
    } else {
      void vscode.window.showInformationMessage("java-vsix-lite: no missing dependencies detected.");
    }
    return;
  }

  const repo = configuredRepository();
  if (repo === undefined) {
    return; // invalid setting — refuse, never fall back to Central silently
  }
  if (!(await confirmDownloadConsent(initial, repo.host))) {
    return;
  }

  await startDownloadProgress(activeClient, initial, repo.base);
}

/**
 * The bounded download loop wrapped in a cancellable progress notification —
 * factored out of `runDownloadDependencies` so the M7 proactive path (which
 * has its own, lighter consent step — see `maybeProactiveDependencyCheck`)
 * can drive the same verified-HTTPS, capped, fixed-point machinery without
 * showing the manual command's heavier modal dialog on top.
 */
async function startDownloadProgress(
  activeClient: LanguageClient,
  initial: MissingDependenciesResult,
  repoBase: string,
): Promise<void> {
  await vscode.window.withProgress(
    {
      location: vscode.ProgressLocation.Notification,
      title: "java-vsix-lite: downloading dependencies",
      cancellable: true,
    },
    (progress, token) => runDownloadLoop(activeClient, initial, progress, token, repoBase),
  );
}

// M7: proactive dependency detection — fires at most once per extension
// session (see `proactiveDependencyCheckDone`), the first time diagnostics
// are published after startup (a reliable, already-happening signal that
// the classpath has been resolved at least once — see `Backend::classpath`'s
// lazy-build-on-first-use doc comment; querying any earlier could race a
// still-empty classpath). Trust-gated and single-flight-guarded exactly like
// the manual command, since it can trigger the same network + `~/.m2` write.
let proactiveDependencyCheckDone = false;

// M7 (fixed): set by the diagnostics middleware the first time the server
// publishes diagnostics — the "classpath built at least once" trigger for
// the proactive check. Session-scoped like `proactiveDependencyCheckDone`
// (the check itself is one-shot regardless).
let proactiveTriggerFired = false;

type AutoDownloadSetting = "prompt" | "always" | "never";

function autoDownloadSetting(): AutoDownloadSetting {
  return vscode.workspace
    .getConfiguration("java-vsix-lite")
    .get<AutoDownloadSetting>("dependencies.autoDownload", "prompt");
}

async function persistAutoDownloadSetting(value: AutoDownloadSetting): Promise<void> {
  try {
    await vscode.workspace
      .getConfiguration("java-vsix-lite")
      .update("dependencies.autoDownload", value, vscode.ConfigurationTarget.Workspace);
  } catch {
    // No open workspace folder (single-file mode) or another write race —
    // the choice still applies to *this* session via the local variable in
    // `maybeProactiveDependencyCheck`; only persistence across sessions is
    // lost, which is a soft failure, not worth surfacing to the user.
  }
}

async function maybeProactiveDependencyCheck(activeClient: LanguageClient): Promise<void> {
  if (proactiveDependencyCheckDone || !vscode.workspace.isTrusted || downloadInFlight) {
    return;
  }
  proactiveDependencyCheckDone = true;

  const setting = autoDownloadSetting();
  if (setting === "never") {
    return;
  }

  let initial: MissingDependenciesResult;
  try {
    initial = await activeClient.sendRequest<MissingDependenciesResult>(
      "jvl/missingDependencies",
    );
  } catch {
    return; // silent — this is a background convenience check, not a user action
  }
  if (initial.missing.length === 0) {
    return;
  }

  const repo = configuredRepository();
  if (repo === undefined) {
    return; // invalid repository setting — the error message already showed
  }

  if (setting === "always") {
    downloadInFlight = true;
    try {
      await startDownloadProgress(activeClient, initial, repo.base);
    } finally {
      downloadInFlight = false;
    }
    return;
  }

  // "prompt": one lightweight (non-modal) notification — deliberately not
  // the manual command's heavier modal dialog, since this fires
  // unprompted. "Always"/"Never" persist the choice for future sessions in
  // this workspace; dismissing the notification (no button) asks again
  // next session rather than silently deciding "never".
  const count = initial.missing.length;
  const choice = await vscode.window.showInformationMessage(
    `java-vsix-lite: ${count} missing dependenc${count === 1 ? "y" : "ies"} can be downloaded from ${repo.host}.`,
    "Download",
    "Always for this workspace",
    "Never",
  );
  if (choice === undefined) {
    return;
  }
  if (choice === "Never") {
    await persistAutoDownloadSetting("never");
    return;
  }
  if (choice === "Always for this workspace") {
    await persistAutoDownloadSetting("always");
  }
  downloadInFlight = true;
  try {
    await startDownloadProgress(activeClient, initial, repo.base);
  } finally {
    downloadInFlight = false;
  }
}

/**
 * M8f (Foundry): the effective download repository. Read from the
 * machine-scoped `dependencies.repository` setting (an internal Maven proxy
 * for governed networks); empty means Maven Central. `undefined` means the
 * configured value is invalid (non-HTTPS, credentials, query/fragment) —
 * callers must refuse to download, with the message below, never fall back
 * to Central silently (the user's intent was clearly "not the internet").
 */
function configuredRepository(): { base: string; host: string } | undefined {
  const raw = vscode.workspace
    .getConfiguration("java-vsix-lite")
    .get<string>("dependencies.repository", "");
  const base = mavenFetch.normalizeRepositoryBase(raw);
  if (base === undefined) {
    void vscode.window.showErrorMessage(
      "java-vsix-lite: the java-vsix-lite.dependencies.repository setting must be a plain https:// URL " +
        "(no credentials, query, or fragment). Downloads are disabled until it is fixed or cleared.",
    );
    return undefined;
  }
  return { base, host: new URL(base).host };
}

/**
 * The single consent dialog the brief requires: modal, names the artifacts
 * (capped display), states the source and destination, and notes that
 * transitives may follow under this same consent. Cancel (or dismissing the
 * dialog) does nothing — only the "Download" choice proceeds.
 */
async function confirmDownloadConsent(
  initial: MissingDependenciesResult,
  repoHost: string,
): Promise<boolean> {
  const shown = initial.missing.slice(0, CONSENT_DISPLAY_CAP).map(coordLabel);
  const more = initial.missing.length - shown.length;
  const list = shown.join("\n") + (more > 0 ? `\n… and ${more} more` : "");
  const skippedNote =
    initial.skipped.length > 0
      ? `\n\n${initial.skipped.length} other degraded dependency(ies) can't be downloaded automatically and will be left as-is.`
      : "";
  const detail =
    `This downloads ${initial.missing.length} artifact(s) over HTTPS from ` +
    `${repoHost} and installs them into ~/.m2/repository:\n\n${list}\n\n` +
    "A downloaded artifact's own transitive dependencies may be discovered and downloaded " +
    "automatically afterward, under this same consent. Every file's checksum is verified " +
    `before it's installed; nothing downloaded is ever executed.${skippedNote}`;
  const choice = await vscode.window.showWarningMessage(
    "java-vsix-lite: Download Missing Dependencies?",
    { modal: true, detail },
    "Download",
  );
  return choice === "Download";
}

/**
 * The bounded fixed-point loop: download this round's missing coordinates,
 * ask the server to rebuild the classpath, re-query for newly-surfaced
 * transitives, repeat — until nothing's left, a round makes no progress, a
 * bound is hit, or the user cancels. Cancellation is only ever observed
 * between artifacts (see the check before each `fetchAndInstallArtifact`
 * call): a coordinate already in flight always finishes installing (both
 * files) or fails cleanly, so no partial file is ever left at its `~/.m2`
 * path.
 */
async function runDownloadLoop(
  activeClient: LanguageClient,
  initial: MissingDependenciesResult,
  progress: vscode.Progress<{ message?: string; increment?: number }>,
  token: vscode.CancellationToken,
  repoBase: string,
): Promise<void> {
  const m2Root = path.join(os.homedir(), ".m2", "repository");
  const downloaded: string[] = [];
  const failed: { coord: string; reason: string }[] = [];
  const attempted = new Set<string>();
  let totalBytes = 0;
  let capNote: string | undefined;
  let cancelled = false;
  // Whether anything has been installed since the last successful rebuild —
  // set on every successful install, cleared once a rebuild for it runs.
  // Tracked separately from the in-loop rebuild below so that a cap/cancel
  // exit (which `break`s out before reaching that rebuild) still gets one
  // final rebuild for whatever *did* install — leaving downloaded jars
  // sitting in `~/.m2` unindexed until a later, unrelated rebuild would be a
  // needless surprise for the user.
  let needsRebuild = false;

  let pending = initial.missing;
  let round = 0;

  roundLoop: while (round < MAX_DOWNLOAD_ROUNDS && pending.length > 0) {
    round++;
    let downloadedThisRound = 0;

    for (const coord of pending) {
      const key = coordLabel(coord);
      if (attempted.has(key)) {
        continue; // already tried (success or failure) in an earlier round
      }
      if (token.isCancellationRequested) {
        cancelled = true;
        break roundLoop;
      }
      if (attempted.size >= MAX_ARTIFACTS_PER_INVOCATION) {
        capNote = `stopped at the ${MAX_ARTIFACTS_PER_INVOCATION}-artifact cap for one invocation`;
        break roundLoop;
      }
      const remainingBytes = MAX_TOTAL_BYTES_PER_INVOCATION - totalBytes;
      if (remainingBytes <= 0) {
        capNote = `stopped at the ${formatBytes(MAX_TOTAL_BYTES_PER_INVOCATION)} download-size cap for one invocation`;
        break roundLoop;
      }

      attempted.add(key);
      progress.report({ message: key });
      const outcome = await mavenFetch.fetchAndInstallArtifact(
        coord,
        m2Root,
        remainingBytes,
        repoBase,
      );
      if (outcome.status === "downloaded") {
        downloaded.push(key);
        downloadedThisRound++;
        totalBytes += outcome.bytes;
        needsRebuild = true;
      } else {
        failed.push({ coord: key, reason: outcome.reason });
      }
    }

    if (token.isCancellationRequested) {
      cancelled = true;
      break;
    }
    if (downloadedThisRound === 0) {
      break; // nothing installed this round — a rebuild/re-query can't surface anything new
    }

    try {
      await activeClient.sendRequest(ExecuteCommandRequest.type, {
        command: SERVER_REBUILD_CLASSPATH_COMMAND,
        arguments: [],
      });
      needsRebuild = false;
      const next = await activeClient.sendRequest<MissingDependenciesResult>(
        "jvl/missingDependencies",
      );
      pending = next.missing;
    } catch (err) {
      capNote = `stopped: classpath rebuild failed (${String(err)})`;
      break;
    }
  }

  if (needsRebuild) {
    // Best-effort: a cap or cancellation cut the loop short after an install
    // — still surface it to IntelliSense rather than leaving a downloaded
    // jar sitting unindexed. Failure here doesn't change the summary; the
    // next build-file change or restart would pick it up regardless.
    await activeClient
      .sendRequest(ExecuteCommandRequest.type, {
        command: SERVER_REBUILD_CLASSPATH_COMMAND,
        arguments: [],
      })
      .catch(() => undefined);
  }

  if (!capNote && !cancelled && round >= MAX_DOWNLOAD_ROUNDS && pending.length > 0) {
    capNote = `stopped at the ${MAX_DOWNLOAD_ROUNDS}-round fixed-point cap`;
  }

  reportDownloadSummary(downloaded, failed, capNote, cancelled);
}

function formatBytes(bytes: number): string {
  return `${Math.round(bytes / (1024 * 1024))}MB`;
}

function summarizeSkipped(skipped: ServerSkippedDependency[]): string {
  const shown = skipped
    .slice(0, CONSENT_DISPLAY_CAP)
    .map((s) => `${s.group}:${s.artifact}${s.version ? `:${s.version}` : ""} (${s.reason})`);
  const more = skipped.length - shown.length;
  return shown.join("; ") + (more > 0 ? `; and ${more} more` : "");
}

function reportDownloadSummary(
  downloaded: string[],
  failed: { coord: string; reason: string }[],
  capNote: string | undefined,
  cancelled: boolean,
): void {
  const parts = [`downloaded ${downloaded.length}`];
  if (failed.length > 0) {
    const detail = failed.map((f) => `${f.coord} (${f.reason})`).join("; ");
    parts.push(`failed ${failed.length}: ${detail}`);
  }
  if (cancelled) {
    parts.push("cancelled by user");
  }
  if (capNote) {
    parts.push(capNote);
  }
  const message = `java-vsix-lite: ${parts.join(" — ")}.`;
  if (failed.length > 0 || cancelled) {
    void vscode.window.showWarningMessage(message);
  } else {
    void vscode.window.showInformationMessage(message);
  }
}

function updateStatus(state: State): void {
  switch (state) {
    case State.Starting:
      statusBar.text = "$(loading~spin) Java Lite";
      statusBar.tooltip = "java-vsix-lite: starting…";
      break;
    case State.Running:
      statusBar.text = "$(check) Java Lite";
      statusBar.tooltip = "java-vsix-lite: default tier active";
      break;
    case State.Stopped:
      statusBar.text = "$(warning) Java Lite";
      statusBar.tooltip = "java-vsix-lite: stopped";
      break;
  }
}
