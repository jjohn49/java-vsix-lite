// Thin extension shell for java-vsix-lite: resolves and launches the Rust
// LSP server over stdio, surfaces its state in the status bar, and wires a
// few commands. All parsing/lint/IntelliSense logic lives in the Rust server.

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
import { activateStyledHover, relocateRelatedInformation } from "./styledHover";
import { activateTesting, TestingApi } from "./testing";
import { createJavacScheduler, JavacScheduler } from "./javacScheduler";

// Result shape mirrors the server's `checkProject` response. javac
// invariants: `-proc:none` mandatory, JDK never auto-downloaded, explicit
// invocation only.
// status: "ok" | "already-running" | "stale" | "jdk-too-old"
//       | "unsupported-layout" | "javac-not-found" | "timeout" | "error"
interface CheckProjectResult {
  status: string;
  message?: string;
  errorCount?: number;
  warningCount?: number;
}

// The user-facing command (contributed in package.json, trust-gated below).
const CHECK_PROJECT_COMMAND = "java-vsix-lite.checkProject";
// The server-internal executeCommand id it forwards to. Must differ from
// CHECK_PROJECT_COMMAND, or vscode-languageclient's auto-registered command
// collides with this extension's own and throws at startup.
const SERVER_CHECK_PROJECT_COMMAND = "jvl.checkProject.run";

// The consent-gated dependency download command. Same collision-avoidance
// pattern as above: this id and `SERVER_REBUILD_CLASSPATH_COMMAND` must
// never be the same string.
const DOWNLOAD_DEPENDENCIES_COMMAND = "java-vsix-lite.downloadDependencies";
const INSTALL_DEPENDENCIES_COMMAND = "java-vsix-lite.installDependencies";
const SERVER_REBUILD_CLASSPATH_COMMAND = "jvl.classpath.rebuild";

// User-facing "refresh": re-reads build files and local caches, republishes
// diagnostics, without restarting the server. Offline only, so unlike the
// download/install commands it is not trust-gated.
const REBUILD_CLASSPATH_COMMAND = "java-vsix-lite.rebuildClasspath";

// Hard ceiling on a build-tool run before it's killed; a hung process must
// not block forever.
const INSTALL_TIMEOUT_MS = 15 * 60 * 1000;

// Fixed-point loop bounds: a deep transitive graph must never turn one
// consented download into an unbounded one.
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
let javacScheduler: JavacScheduler | undefined;
let clientGeneration = 0; // bumped in start(); a run from an old client never touches a new one

function hasDirtyJavaBuffers(): boolean {
  return vscode.workspace.textDocuments.some(
    (d) => d.languageId === "java" && d.uri.scheme === "file" && d.isDirty,
  );
}

/** Forwards saved URIs to the server's javac check, silently — unlike the
 * manual command, which reports errors. `busy` means requeue the URIs for
 * the next dispatch. */
async function runBackgroundCheck(uris: readonly string[]): Promise<"completed" | "busy"> {
  const generation = clientGeneration;
  const c = client;
  if (!c) return "completed";
  try {
    const result = (await c.sendRequest(ExecuteCommandRequest.type, {
      command: SERVER_CHECK_PROJECT_COMMAND,
      arguments: [{ scope: "modules", documentUris: [...uris] }],
    })) as CheckProjectResult;
    if (generation !== clientGeneration) return "completed";
    // `stale`: the run was outrun by a provider change, so the saved files
    // still have not been compiled against current state — requeue.
    if (result.status === "already-running" || result.status === "stale") return "busy";
    if (result.status === "jdk-too-old") {
      if (!jdkTooOldNotified) {
        jdkTooOldNotified = true;
        notifyJdkTooOld(result);
      }
    } else {
      jdkTooOldNotified = false;
    }
    return "completed";
  } catch {
    return "completed"; // silent by design; the manual command reports errors
  }
}

// ---------------------------------------------------------------------------
// Status bar: one renderer owns every mutation; nothing else writes
// `statusBar.*` directly. State is composed from server lifecycle,
// transient activity text, and live diagnostic counts.
// ---------------------------------------------------------------------------

type ServerStateKind = "starting" | "running" | "stopped" | "missing-binary";

const SERVER_STATE_LABEL: Record<ServerStateKind, string> = {
  starting: "starting…",
  running: "default tier active",
  stopped: "stopped",
  "missing-binary": "jvl-server binary not found",
};

let serverState: ServerStateKind = "starting";
/** Transient activity shown with a spinner (e.g. "checking project (javac)…"). */
let statusActivity: string | undefined;

function setServerState(state: ServerStateKind): void {
  serverState = state;
  renderStatusBar();
}

function setStatusActivity(activity: string | undefined): void {
  statusActivity = activity;
  renderStatusBar();
}

/** Error/warning counts over every Java file's diagnostics from this
 * extension's sources (`java-vsix-lite` native tier or `javac`). */
function javaDiagnosticCounts(): { errors: number; warnings: number } {
  let errors = 0;
  let warnings = 0;
  for (const [uri, diagnostics] of vscode.languages.getDiagnostics()) {
    if (!uri.path.endsWith(".java")) {
      continue;
    }
    for (const diagnostic of diagnostics) {
      if (diagnostic.source !== "java-vsix-lite" && diagnostic.source !== "javac") {
        continue;
      }
      if (diagnostic.severity === vscode.DiagnosticSeverity.Error) {
        errors += 1;
      } else if (diagnostic.severity === vscode.DiagnosticSeverity.Warning) {
        warnings += 1;
      }
    }
  }
  return { errors, warnings };
}

function renderStatusBar(): void {
  if (!statusBar) {
    return;
  }
  const { errors, warnings } = javaDiagnosticCounts();
  const icon = statusActivity
    ? "$(loading~spin)"
    : {
        starting: "$(loading~spin)",
        running: "$(check)",
        stopped: "$(warning)",
        "missing-binary": "$(error)",
      }[serverState];
  const counts =
    errors > 0 || warnings > 0 ? ` $(error) ${errors} $(warning) ${warnings}` : "";
  statusBar.text = `${icon} Java Lite${counts}`;
  statusBar.backgroundColor =
    errors > 0
      ? new vscode.ThemeColor("statusBarItem.errorBackground")
      : warnings > 0
        ? new vscode.ThemeColor("statusBarItem.warningBackground")
        : undefined;

  const jdkHome =
    vscode.workspace.getConfiguration("java-vsix-lite").get<string>("jdk.home") ||
    process.env.JAVA_HOME ||
    "auto-discovered";
  const tooltip = new vscode.MarkdownString(undefined, true);
  tooltip.appendMarkdown(
    `**java-vsix-lite**: ${statusActivity ?? SERVER_STATE_LABEL[serverState]}\n\n`,
  );
  tooltip.appendMarkdown(`$(error) ${errors} errors · $(warning) ${warnings} warnings (Java)\n\n`);
  tooltip.appendMarkdown(`JDK: ${jdkHome}`);
  statusBar.tooltip = tooltip;
}

// Read-only virtual documents for external (JDK/dependency) goto-definition
// targets. Fetches content on demand via `jvl/externalSource`: real source
// when available, else a signature-only stub.
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

/** The extension's exported API (what `extension.activate()` resolves to) —
 * consumed by the Electron test suites to drive the Test Explorer. */
export interface ExtensionApi {
  testing: TestingApi;
}

export async function activate(context: vscode.ExtensionContext): Promise<ExtensionApi> {
  statusBar = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Left, 0);
  renderStatusBar();
  statusBar.show();
  context.subscriptions.push(statusBar);
  // Live error/warning counts and background color track the editor's
  // diagnostics collection.
  context.subscriptions.push(vscode.languages.onDidChangeDiagnostics(() => renderStatusBar()));

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

  // Automatic javac backstop: batches saved Java files, dispatching at
  // most once every 30s, deferring while any buffer is dirty (javac
  // compiles disk, not the buffer). Never runs on open/load.
  javacScheduler = createJavacScheduler({
    now: () => performance.now(),
    setTimer: (cb, ms) => setTimeout(cb, ms),
    clearTimer: (h) => clearTimeout(h as NodeJS.Timeout),
    enabled: () => javacBackgroundCheckEnabled() && client !== undefined,
    blocked: hasDirtyJavaBuffers,
    run: runBackgroundCheck,
  });
  context.subscriptions.push(
    vscode.workspace.onDidSaveTextDocument((doc) => {
      if (doc.languageId === "java" && doc.uri.scheme === "file" && javacBackgroundCheckEnabled()) {
        javacScheduler?.enqueue(doc.uri.toString());
      } else {
        javacScheduler?.poke(); // a dirty buffer became clean: unblock a queued batch
      }
    }),
    vscode.workspace.onDidCloseTextDocument((doc) => {
      if (doc.languageId === "java") {
        javacScheduler?.poke();
      }
    }),
    vscode.workspace.onDidChangeConfiguration((e) => {
      if (e.affectsConfiguration("java-vsix-lite.javac.checkOnSave")) {
        if (javacBackgroundCheckEnabled()) {
          javacScheduler?.poke();
        } else {
          javacScheduler?.cancelPending();
        }
      }
    }),
    { dispose: () => javacScheduler?.dispose() },
  );

  // Granting trust mid-session unlocks the background check; poke in case a
  // batch was queued and blocked on the trust gate. Never a full open-docs
  // sweep — automatic javac must never run on open/load.
  context.subscriptions.push(
    vscode.workspace.onDidGrantWorkspaceTrust(() => {
      javacScheduler?.poke();
    }),
  );

  // Pin the language id explicitly for jvl-src docs: documentSelector match
  // (and thus server sync) depends on it, and auto-detection can miss.
  context.subscriptions.push(
    vscode.workspace.onDidOpenTextDocument((doc) => {
      if (doc.uri.scheme === "jvl-src" && doc.languageId !== "java") {
        void vscode.languages.setTextDocumentLanguage(doc, "java");
      }
    }),
  );

  // The DAP adapter reuses the same machine-scoped jvl-server binary, so a
  // workspace can never redirect which binary debugs it. The configuration
  // provider enforces the Workspace Trust gate since debugging runs project
  // code.
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

  // JUnit test support: Test Explorer discovery via the server's `jvl/tests`
  // request, trust-gated run/debug through the DAP adapter (see testing.ts).
  const testing = activateTesting(context, () => client);

  // Styled diagnostic hovers: severity-colored re-rendering of this
  // extension's diagnostics with code-styled type names and clickable
  // declaration links (see styledHover.ts).
  activateStyledHover(context);

  await start(context);
  return { testing };
}

export async function deactivate(): Promise<void> {
  javacScheduler?.dispose();
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

    // Inject the machine-scoped JDK home, overwriting anything workspace-
    // provided: a workspace launch.json must never redirect which JVM
    // binary runs. Undefined falls back to $JAVA_HOME, then filesystem
    // discovery.
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

// Verify the bundled server version asynchronously; mismatches warn but never
// block activation.
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
    setServerState("missing-binary");
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
    // `jvl-src` is included so ExternalSourceProvider's virtual documents
    // sync to the server too, keeping hover/definition/semantic tokens
    // working for external/JDK source.
    documentSelector: [
      { scheme: "file", language: "java" },
      { scheme: "jvl-src", language: "java" },
    ],
    outputChannelName: "java-vsix-lite",
    initializationOptions: {
      unresolvedMemberDiagnostics: vscode.workspace
        .getConfiguration("java-vsix-lite")
        .get<boolean>("diagnostics.unresolvedMembers", true),
      unusedDiagnostics: vscode.workspace
        .getConfiguration("java-vsix-lite")
        .get<boolean>("diagnostics.unused", true),
      // Explicit override for where to find javac, tried before $JAVA_HOME
      // (empty string means "unset").
      jdkHome: vscode.workspace.getConfiguration("java-vsix-lite").get<string>("jdk.home", ""),
      javacTimeoutSecs: vscode.workspace
        .getConfiguration("java-vsix-lite")
        .get<number>("javac.timeoutSecs", 120),
    },
    middleware: {
      // The first publishDiagnostics after startup triggers the proactive
      // dependency check. Must stay middleware, never
      // client.onNotification(...): jsonrpc keeps one handler per method, so
      // a second handler would replace the client's diagnostics handling and
      // kill every squiggle.
      handleDiagnostics: (uri, diagnostics, next) => {
        // Move relatedInformation into the styled hover so the plain hover
        // block shows only the single message line VS Code renders.
        relocateRelatedInformation(uri, diagnostics);
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

  clientGeneration += 1;
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
  javacScheduler?.reset();
  await client?.stop();
  client = undefined;
  await start(context);
}

// Light "refresh": rebuilds the classpath and republishes diagnostics
// without restarting the server. Offline and side-effect-free, so no
// trust gate.
async function rebuildClasspath(): Promise<void> {
  if (!client) {
    void vscode.window.showErrorMessage("java-vsix-lite: the language server is not running.");
    return;
  }

  setStatusActivity("rebuilding classpath…");
  try {
    await client.sendRequest(ExecuteCommandRequest.type, {
      command: SERVER_REBUILD_CLASSPATH_COMMAND,
      arguments: [],
    });
    void vscode.window.showInformationMessage("java-vsix-lite: classpath rebuilt.");
    // Re-evaluate the scheduler so a batch waiting on a stale classpath can
    // dispatch; never enqueues anything new.
    javacScheduler?.poke();
  } catch (err) {
    void vscode.window.showErrorMessage(
      `java-vsix-lite: could not rebuild the classpath: ${String(err)}`,
    );
  } finally {
    setStatusActivity(undefined);
  }
}

// One-shot, trust-gated javac check: refuses outright in an untrusted
// workspace since spawning javac compiles project code. The server has no
// notion of Workspace Trust, so this is the only enforcement and is
// load-bearing.
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

  setStatusActivity("checking project (javac)…");
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
    setStatusActivity(undefined);
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
    case "stale":
      void vscode.window.showInformationMessage(
        "java-vsix-lite: the workspace changed while Check Project was running; run it again.",
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
// javac check was skipped. Surface it as a notification too, with a
// shortcut to the JDK override setting.
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

// The background check is silent, but a JDK-too-old config problem is
// surfaced once, not on every save. Reset when a check stops reporting it,
// so fixing then re-breaking notifies again.
let jdkTooOldNotified = false;

// True while a download is in flight, start to end of the loop. Mirrors
// `checkProject`'s single-flight pattern; no queuing — a second invocation
// is just told one is already running.
let downloadInFlight = false;

// Running Maven or Gradle executes workspace code, so installation is trusted,
// explicit, and never automatic. It fills local caches for unresolved versions.

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
 * Locate the build tool for `root`: Maven if `pom.xml` is present, else
 * Gradle if a Gradle build file is. Prefers the project wrapper, then a
 * configured path, then `PATH`; `undefined` if neither applies.
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
 * Pick the executable: project wrapper, else configured path, else PATH or
 * common install dirs, else the bare name. Probes common dirs because a
 * GUI-launched editor's PATH often omits Homebrew/SDKMAN.
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
    javacScheduler?.poke();
  }
}

/**
 * Run `tool`, streaming output to `channel`, killed on cancellation or
 * timeout. Uses `spawn` with no shell so a crafted path can't inject extra
 * commands.
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

// Consent-gated dependency download: trust-gated like `checkProject`
// (network I/O + writes to `~/.m2`), then `jvl/missingDependencies` -> one
// modal consent dialog -> a bounded fixed-point download/rebuild loop.
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
  // `activeClient` is captured once by the caller: `client` is mutable
  // module state that a restart could swap out from under a long-running
  // download.
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
      // These couldn't be resolved to a downloadable g:a:v — usually a
      // version managed by a parent POM/BOM missing from the local cache.
      // Direct download can't help; running the build tool once can.
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
 * The bounded download loop wrapped in a cancellable progress notification.
 * Factored out so the proactive path can drive the same capped, fixed-point
 * machinery without the manual command's heavier modal dialog.
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

// Proactive dependency detection: fires once per session, on the first
// diagnostics publish after startup (a signal the classpath has been built
// at least once — querying earlier could race an empty classpath).
// Trust-gated and single-flight-guarded like the manual command.
let proactiveDependencyCheckDone = false;

// Set by the diagnostics middleware on the first publish — the "classpath
// built at least once" trigger for the proactive check.
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
    // No open workspace folder, or a write race — the choice still applies
    // to this session; only cross-session persistence is lost, a soft
    // failure not worth surfacing.
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

  // "prompt": one lightweight, non-modal notification since this fires
  // unprompted. "Always"/"Never" persist the choice; dismissing without a
  // button asks again next session rather than defaulting to "never".
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
 * The effective download repository, from the machine-scoped
 * `dependencies.repository` setting (empty means Maven Central).
 * `undefined` means the value is invalid; callers must refuse to download,
 * never fall back to Central silently.
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
 * The single consent dialog: modal, names the artifacts (capped), states
 * source/destination, and notes transitives may follow under this consent.
 * Only the "Download" choice proceeds.
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
 * Bounded fixed-point loop: download missing coordinates, rebuild the
 * classpath, re-query for transitives, repeat until done, stalled, capped,
 * or cancelled. Cancellation is only observed between artifacts, so an
 * in-flight coordinate always finishes or fails cleanly — never a partial
 * file in `~/.m2`.
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
  // Whether anything installed since the last rebuild. Tracked separately
  // so a cap/cancel exit still gets one final rebuild for whatever did
  // install, rather than leaving jars unindexed.
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
    // Best-effort: a cap or cancellation cut the loop short after an
    // install, so surface it to IntelliSense rather than leave a jar
    // unindexed. Failure here doesn't change the summary.
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
      setServerState("starting");
      break;
    case State.Running:
      setServerState("running");
      break;
    case State.Stopped:
      setServerState("stopped");
      break;
  }
}
