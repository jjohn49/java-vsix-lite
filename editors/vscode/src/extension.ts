// Thin extension shell for java-vsix-lite.
//
// Per the implementation plan's "Process topology", this shell does NOT contain
// analysis logic. Its entire job is: resolve and launch the single Rust LSP
// server over stdio, surface its state in the status bar, and wire a couple of
// commands. All parsing/lint/IntelliSense (and, later, supervision of the
// optional javac tier) lives inside the Rust server.

import { execFile } from "child_process";
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
const SERVER_REBUILD_CLASSPATH_COMMAND = "jvl.classpath.rebuild";

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
    vscode.commands.registerCommand(DOWNLOAD_DEPENDENCIES_COMMAND, async () => {
      await downloadDependencies();
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
      command: SERVER_CHECK_PROJECT_COMMAND,
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

// M6.2: one download invocation at a time — `true` while one is in flight
// (from the missing-deps query through the end of the download loop). The
// extension-side mirror of `checkProject`'s single-flight pattern (the server
// enforces that one via `javac_running`; downloads are driven entirely by
// this process, so the flag lives here). No queuing: a second invocation is
// simply told one is already running.
let downloadInFlight = false;

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
      void vscode.window.showInformationMessage(
        `java-vsix-lite: no missing dependencies can be downloaded automatically. ` +
          `${initial.skipped.length} dependency(ies) skipped: ${summarizeSkipped(initial.skipped)}.`,
      );
    } else {
      void vscode.window.showInformationMessage("java-vsix-lite: no missing dependencies detected.");
    }
    return;
  }

  if (!(await confirmDownloadConsent(initial))) {
    return;
  }

  await vscode.window.withProgress(
    {
      location: vscode.ProgressLocation.Notification,
      title: "java-vsix-lite: downloading dependencies",
      cancellable: true,
    },
    (progress, token) => runDownloadLoop(activeClient, initial, progress, token),
  );
}

/**
 * The single consent dialog the brief requires: modal, names the artifacts
 * (capped display), states the source and destination, and notes that
 * transitives may follow under this same consent. Cancel (or dismissing the
 * dialog) does nothing — only the "Download" choice proceeds.
 */
async function confirmDownloadConsent(initial: MissingDependenciesResult): Promise<boolean> {
  const shown = initial.missing.slice(0, CONSENT_DISPLAY_CAP).map(coordLabel);
  const more = initial.missing.length - shown.length;
  const list = shown.join("\n") + (more > 0 ? `\n… and ${more} more` : "");
  const skippedNote =
    initial.skipped.length > 0
      ? `\n\n${initial.skipped.length} other degraded dependency(ies) can't be downloaded automatically and will be left as-is.`
      : "";
  const detail =
    `This downloads ${initial.missing.length} artifact(s) over HTTPS from Maven Central ` +
    `(repo.maven.apache.org) and installs them into ~/.m2/repository:\n\n${list}\n\n` +
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
      const outcome = await mavenFetch.fetchAndInstallArtifact(coord, m2Root, remainingBytes);
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
