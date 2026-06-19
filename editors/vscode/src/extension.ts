// Thin extension shell for java-vsix-lite.
//
// Per the implementation plan's "Process topology", this shell does NOT contain
// analysis logic. Its entire job is: resolve and launch the single Rust LSP
// server over stdio, surface its state in the status bar, and wire a couple of
// commands. All parsing/lint/IntelliSense (and, later, supervision of the
// optional javac tier) lives inside the Rust server.

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
      options: { env: { ...process.env, JVL_LOG: "debug" } },
    },
  };

  const clientOptions: LanguageClientOptions = {
    documentSelector: [{ scheme: "file", language: "java" }],
    outputChannelName: "java-vsix-lite",
  };

  client = new LanguageClient(
    "java-vsix-lite",
    "java-vsix-lite",
    serverOptions,
    clientOptions,
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
