// Thin extension shell for java-vsix-lite.
//
// Per the implementation plan's "Process topology", this shell does NOT contain
// analysis logic. Its entire job is: resolve and launch the single Rust LSP
// server over stdio, surface its state in the status bar, and wire a couple of
// commands. All parsing/lint/IntelliSense (and, later, supervision of the
// optional javac tier) lives inside the Rust server.

import { execFile } from "child_process";
import * as fs from "fs";
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

const CHECK_PROJECT_COMMAND = "java-vsix-lite.checkProject";

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
    vscode.workspace.registerTextDocumentContentProvider("jvl-src", new ExternalSourceProvider()),
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
}

async function restart(context: vscode.ExtensionContext): Promise<void> {
  await client?.stop();
  client = undefined;
  await start(context);
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
      command: CHECK_PROJECT_COMMAND,
      arguments: [],
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
    default:
      void vscode.window.showErrorMessage(
        `java-vsix-lite: Check Project failed: ${result.message ?? result.status}`,
      );
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
