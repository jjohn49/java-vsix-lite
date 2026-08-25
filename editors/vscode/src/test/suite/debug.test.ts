// Trusted-workspace debug E2E: `vscode.debug.startDebugging` with a
// `java-vsix-lite` launch config starts a real session through the bundled
// adapter (`jvl-server dap`, resolved via the same JVL_SERVER_PATH /
// machine-setting / bundled precedence as the language server) and the
// program runs to completion. Protocol depth (breakpoints, stepping,
// variables, attach) is covered by the Rust integration tests in
// `crates/server/tests/dap.rs`; this asserts the extension-side wiring.
//
// The fixture `demo/App.java` is compiled in `suiteSetup` with a `javac`
// located via `$JAVA_HOME` or `PATH`; if none is found the suite skips
// (CI is expected to have a JDK — the Rust tests already require one).
import * as assert from "assert";
import { spawnSync } from "child_process";
import * as fs from "fs";
import * as os from "os";
import * as path from "path";
import * as vscode from "vscode";

const FIXTURE = path.resolve(__dirname, "../../../test-fixture");

function locateJavac(): string | undefined {
  const exe = process.platform === "win32" ? "javac.exe" : "javac";
  const javaHome = process.env.JAVA_HOME;
  if (javaHome) {
    const candidate = path.join(javaHome, "bin", exe);
    if (fs.existsSync(candidate)) {
      return candidate;
    }
  }
  const onPath = spawnSync(exe, ["-version"], { encoding: "utf8" });
  return onPath.status === 0 ? exe : undefined;
}

/** Poll until `predicate` holds or `timeoutMs` elapses (same shape as
 *  `diagnostics.test.ts`'s helper; real-clock polling by necessity — the
 *  observed state is driven by processes outside this extension host). */
async function waitFor(predicate: () => boolean, timeoutMs: number): Promise<boolean> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (predicate()) {
      return true;
    }
    await new Promise((r) => setTimeout(r, 250));
  }
  return predicate();
}

suite("debugging (trusted workspace)", () => {
  let classesDir: string | undefined;

  suiteSetup(function () {
    const javac = locateJavac();
    if (!javac) {
      this.skip();
      return;
    }
    classesDir = fs.mkdtempSync(path.join(os.tmpdir(), "jvl-debug-fixture-"));
    const appJava = path.join(FIXTURE, "src", "main", "java", "demo", "App.java");
    // Compile only App.java — Broken.java keeps its deliberate error.
    const result = spawnSync(javac, ["-g", "-d", classesDir, appJava], {
      encoding: "utf8",
    });
    assert.strictEqual(
      result.status,
      0,
      `javac failed to compile the debug fixture: ${result.stderr}`,
    );
  });

  suiteTeardown(() => {
    if (classesDir) {
      fs.rmSync(classesDir, { recursive: true, force: true });
    }
  });

  test("launch config starts a session that runs to completion", async function () {
    if (!classesDir) {
      this.skip();
      return;
    }
    const folder = vscode.workspace.workspaceFolders?.[0];
    assert.ok(folder, "test harness must open the fixture workspace");

    let sessionSeen: vscode.DebugSession | undefined;
    const started = new Promise<void>((resolve) => {
      const sub = vscode.debug.onDidStartDebugSession((session) => {
        if (session.type === "java-vsix-lite") {
          sessionSeen = session;
          sub.dispose();
          resolve();
        }
      });
    });
    const terminated = new Promise<void>((resolve) => {
      const sub = vscode.debug.onDidTerminateDebugSession((session) => {
        if (session.type === "java-vsix-lite") {
          sub.dispose();
          resolve();
        }
      });
    });

    const launched = await vscode.debug.startDebugging(folder, {
      type: "java-vsix-lite",
      name: "t",
      request: "launch",
      mainClass: "demo.App",
      classPaths: [classesDir],
      cwd: FIXTURE,
    });
    assert.strictEqual(launched, true, "startDebugging must resolve true in a trusted workspace");

    // Real wall-clock deadline by necessity: the awaited signals come from
    // a real adapter process and JVM outside this Node process, so fake
    // timers cannot advance them — this only bounds a genuine hang.
    const timeout = (ms: number) =>
      new Promise<never>((_, reject) =>
        setTimeout(() => reject(new Error(`debug session event not seen within ${ms}ms`)), ms),
      );
    await Promise.race([started, timeout(30_000)]);
    assert.strictEqual(sessionSeen?.type, "java-vsix-lite");

    // App.java has no breakpoints set: it runs to completion and the
    // session ends by itself.
    await Promise.race([terminated, timeout(30_000)]);
  });

  test("breakpoint stop jumps the editor to the stopped line", async function () {
    if (!classesDir) {
      this.skip();
      return;
    }
    const folder = vscode.workspace.workspaceFolders?.[0];
    assert.ok(folder, "test harness must open the fixture workspace");
    const appUri = vscode.Uri.file(
      path.join(FIXTURE, "src", "main", "java", "demo", "App.java"),
    );
    // App.java line 7 (0-based 6): the println — inside main, executable.
    const bpLine = 6;
    const bp = new vscode.SourceBreakpoint(
      new vscode.Location(appUri, new vscode.Position(bpLine, 0)),
    );
    vscode.debug.addBreakpoints([bp]);

    // A DAP tracker observes the real wire: the `stopped` event carries the
    // threadId the continue below needs.
    let stoppedThread: number | undefined;
    const stopped = new Promise<void>((resolve) => {
      const sub = vscode.debug.registerDebugAdapterTrackerFactory("java-vsix-lite", {
        createDebugAdapterTracker: () => ({
          onDidSendMessage: (message: {
            type?: string;
            event?: string;
            body?: { reason?: string; threadId?: number };
          }) => {
            if (
              message.type === "event" &&
              message.event === "stopped" &&
              message.body?.reason === "breakpoint"
            ) {
              stoppedThread = message.body.threadId;
              sub.dispose();
              resolve();
            }
          },
        }),
      });
    });
    const terminated = new Promise<void>((resolve) => {
      const sub = vscode.debug.onDidTerminateDebugSession((session) => {
        if (session.type === "java-vsix-lite") {
          sub.dispose();
          resolve();
        }
      });
    });

    // Real wall-clock deadline by necessity (see above): the awaited
    // signals come from a real adapter process and JVM.
    const timeout = (ms: number) =>
      new Promise<never>((_, reject) =>
        setTimeout(() => reject(new Error(`debug stop not seen within ${ms}ms`)), ms),
      );
    try {
      const launched = await vscode.debug.startDebugging(folder, {
        type: "java-vsix-lite",
        name: "t-bp",
        request: "launch",
        mainClass: "demo.App",
        classPaths: [classesDir],
        cwd: FIXTURE,
      });
      assert.strictEqual(launched, true);
      await Promise.race([stopped, timeout(30_000)]);

      // The actual user-visible contract: VS Code focuses an editor on the
      // stopped source line (driven by our stackTrace response's source).
      const revealed = await waitFor(() => {
        const editor = vscode.window.activeTextEditor;
        return (
          editor !== undefined &&
          editor.document.uri.fsPath === appUri.fsPath &&
          editor.selection.active.line === bpLine
        );
      }, 15_000);
      const editor = vscode.window.activeTextEditor;
      assert.ok(
        revealed,
        `editor did not jump to the breakpoint: active=${editor?.document.uri.fsPath} ` +
          `line=${editor?.selection.active.line}, expected ${appUri.fsPath} line ${bpLine}`,
      );

      // Resume; the program runs to completion.
      assert.ok(stoppedThread !== undefined, "stopped event must carry a threadId");
      await vscode.debug.activeDebugSession?.customRequest("continue", {
        threadId: stoppedThread,
      });
      await Promise.race([terminated, timeout(30_000)]);
    } finally {
      vscode.debug.removeBreakpoints([bp]);
    }
  });
});
