// Thin extension shell for java-vsix-lite.
//
// Per the implementation plan's "Process topology", this shell does NOT contain
// analysis logic. Its entire job is: resolve and launch the single Rust LSP
// server over stdio, surface its state in the status bar, and wire a couple of
// commands. All parsing/lint/IntelliSense (and, later, supervision of the
// optional javac tier) lives inside the Rust server.

import { execFileSync } from "child_process";
import * as fs from "fs";
import * as path from "path";
import * as vscode from "vscode";
import {
  LanguageClient,
  LanguageClientOptions,
  ServerOptions,
  State,
  TransportKind,
} from "vscode-languageclient/node";

let client: LanguageClient | undefined;
let statusBar: vscode.StatusBarItem;

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

// Version handshake: runs the server binary with `--version` and warns (via
// the LSP output channel, non-fatally) if it disagrees with the extension's
// own version. Catches a stale bundled binary left over from a partial
// update; never blocks startup — a spawn failure or an older binary that
// doesn't understand `--version` is swallowed as a warning too.
function checkVersionHandshake(
  serverPath: string,
  extensionVersion: string,
  outputChannel: vscode.OutputChannel,
): void {
  try {
    const serverVersion = execFileSync(serverPath, ["--version"], {
      encoding: "utf8",
      timeout: 5000,
    }).trim();
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
    documentSelector: [{ scheme: "file", language: "java" }],
    outputChannelName: "java-vsix-lite",
    initializationOptions: {
      unresolvedMemberDiagnostics: vscode.workspace
        .getConfiguration("java-vsix-lite")
        .get<boolean>("diagnostics.unresolvedMembers", false),
    },
  };

  client = new LanguageClient(
    "java-vsix-lite",
    "java-vsix-lite",
    serverOptions,
    clientOptions,
  );

  checkVersionHandshake(
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
