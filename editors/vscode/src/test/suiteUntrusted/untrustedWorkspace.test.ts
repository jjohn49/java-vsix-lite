// Asserts the "limited" `untrustedWorkspaces` contract declared in
// package.json: in an UNTRUSTED workspace the pure-Rust default tier keeps
// working (semantic tokens, outline/document symbols, activation), while
// every trust-gated feature refuses to run -- the `vscode.workspace.isTrusted`
// guards in `src/extension.ts`:
//   - `javacBackgroundCheckEnabled()` (automatic checkOnSave / on-load check)
//   - `checkProject()` (the `java-vsix-lite.checkProject` command)
//   - `installDependencies()` (`java-vsix-lite.installDependencies`)
//   - `downloadDependencies()` (`java-vsix-lite.downloadDependencies`)
//
// This suite is launched by `runTest.ts` via a SEPARATE `runTests()` call
// than the trusted `../suite/` tests: that call does NOT pass
// `--disable-workspace-trust` (Workspace Trust stays enabled) and it opens
// `test-fixture-untrusted` for the first time in a throwaway profile whose
// seeded `User/settings.json` sets
// `security.workspace.trust.startupPrompt: "never"` -- the opposite launch
// config from the trusted suite, and enough to keep a *real* VS Code window
// untrusted without a blocking modal.
//
// BUT: `@vscode/test-electron` always launches via `--extensionDevelopmentPath`
// (the "Extension Development Host"), and that host is unconditionally
// trusted by VS Code regardless of those settings -- confirmed empirically
// here: even with trust enabled and a never-before-seen folder in a brand
// new profile, `vscode.workspace.isTrusted` still read `true`. There is no
// launch flag that changes this; it is intentional VS Code behavior so that
// F5-debugging an extension is never interrupted by a trust prompt.
//
// To still exercise the REAL trust gates in `extension.ts` (not a mock of
// them), `suiteSetup` below overrides the shared `vscode.workspace.isTrusted`
// getter to return `false` for the duration of this suite. This works
// because the extension host is one Node process: this test file's
// `import * as vscode from "vscode"` and `extension.ts`'s resolve to the
// exact same module instance, so `checkProject()`, `installDependencies()`,
// `downloadDependencies()`, and `javacBackgroundCheckEnabled()` all observe
// the override live and take their genuine untrusted-workspace code paths --
// only the one signal this harness cannot otherwise force is faked; every
// assertion below still exercises unmodified production code.
import * as assert from "assert";
import * as path from "path";
import * as vscode from "vscode";

const FIXTURE = path.resolve(__dirname, "../../../test-fixture-untrusted");

function fixtureUri(...segments: string[]): vscode.Uri {
  return vscode.Uri.file(path.join(FIXTURE, ...segments));
}

async function waitForSemanticTokens(
  uri: vscode.Uri,
  timeoutMs: number,
): Promise<vscode.SemanticTokens | undefined> {
  const deadline = Date.now() + timeoutMs;
  let result: vscode.SemanticTokens | undefined;
  while (Date.now() < deadline) {
    result = await vscode.commands.executeCommand<vscode.SemanticTokens>(
      "vscode.provideDocumentSemanticTokens",
      uri,
    );
    if (result && result.data.length > 0) {
      return result;
    }
    await new Promise((r) => setTimeout(r, 500));
  }
  return result;
}

async function waitForDocumentSymbols(
  uri: vscode.Uri,
  timeoutMs: number,
): Promise<vscode.DocumentSymbol[] | undefined> {
  const deadline = Date.now() + timeoutMs;
  let result: vscode.DocumentSymbol[] | undefined;
  while (Date.now() < deadline) {
    result = await vscode.commands.executeCommand<vscode.DocumentSymbol[]>(
      "vscode.executeDocumentSymbolProvider",
      uri,
    );
    if (result && result.length > 0) {
      return result;
    }
    await new Promise((r) => setTimeout(r, 500));
  }
  return result;
}

/** Poll until `predicate` holds or `timeoutMs` elapses (same shape as the
 *  trusted suite's helper in `../suite/diagnostics.test.ts`). */
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

type ErrorMessageFn = typeof vscode.window.showErrorMessage;

/**
 * Temporarily intercepts `vscode.window.showErrorMessage` to capture the
 * refusal text a trust-gated command shows the user, without a real modal
 * ever appearing (this harness has no UI driver to dismiss one).
 */
async function captureErrorMessage(
  action: () => Thenable<unknown>,
): Promise<string | undefined> {
  const win = vscode.window as unknown as { showErrorMessage: ErrorMessageFn };
  const original = win.showErrorMessage;
  let captured: string | undefined;
  win.showErrorMessage = ((message: string) => {
    captured = message;
    return Promise.resolve(undefined);
  }) as ErrorMessageFn;
  try {
    await action();
  } finally {
    win.showErrorMessage = original;
  }
  return captured;
}

suite("untrusted workspace", () => {
  let originalIsTrustedDescriptor: PropertyDescriptor | undefined;

  suiteSetup(() => {
    originalIsTrustedDescriptor = Object.getOwnPropertyDescriptor(
      vscode.workspace,
      "isTrusted",
    );
    // Force the untrusted signal (see the file header for why the Extension
    // Development Host can't be made genuinely untrusted from launch args
    // alone). Fail loudly, not vacuously, if a future VS Code makes this
    // property non-configurable -- every assertion below depends on it.
    try {
      Object.defineProperty(vscode.workspace, "isTrusted", {
        configurable: true,
        get: () => false,
      });
    } catch (err) {
      throw new Error(
        `could not force vscode.workspace.isTrusted to false (property no longer ` +
          `configurable?) -- this suite can no longer simulate an untrusted workspace: ${String(err)}`,
      );
    }
    assert.strictEqual(
      vscode.workspace.isTrusted,
      false,
      "override of vscode.workspace.isTrusted did not take effect",
    );
  });

  suiteTeardown(() => {
    if (originalIsTrustedDescriptor) {
      Object.defineProperty(vscode.workspace, "isTrusted", originalIsTrustedDescriptor);
    }
  });

  test("default tier: extension activates and serves semantic tokens + outline for a valid file", async () => {
    const ext = vscode.extensions.getExtension("java-vsix-lite.java-vsix-lite");
    assert.ok(ext, "extension not found under id java-vsix-lite.java-vsix-lite");
    if (!ext!.isActive) {
      await ext!.activate();
    }
    assert.ok(ext!.isActive, "extension did not activate in an untrusted workspace");

    const uri = fixtureUri("src", "main", "java", "demo", "Sample.java");
    const doc = await vscode.workspace.openTextDocument(uri);
    await vscode.window.showTextDocument(doc);

    const tokens = await waitForSemanticTokens(uri, 30_000);
    assert.ok(
      tokens && tokens.data.length > 0,
      "expected non-empty semantic tokens for a valid Java file in an untrusted " +
        "workspace -- the pure-Rust default tier must not be gated by trust",
    );

    const symbols = await waitForDocumentSymbols(uri, 30_000);
    assert.ok(
      symbols && symbols.some((s) => s.name === "Sample"),
      `expected a "Sample" class symbol from the outline provider, got: ${JSON.stringify(symbols)}`,
    );
  });

  test("javac tier stays off: no automatic check-on-load diagnostics for a file javac would flag", async () => {
    const uri = fixtureUri("src", "main", "java", "demo", "TypeError.java");
    const doc = await vscode.workspace.openTextDocument(uri);
    await vscode.window.showTextDocument(doc);

    // Give the (would-be) debounced on-load/on-save javac check every chance
    // to fire if the trust gate were broken -- well past its 1.5s debounce.
    await new Promise((r) => setTimeout(r, 5_000));
    const javacDiags = vscode.languages.getDiagnostics(uri).filter((d) => d.source === "javac");
    assert.strictEqual(
      javacDiags.length,
      0,
      "javac diagnostics appeared in an untrusted workspace -- checkOnSave must " +
        "be trust-gated (see javacBackgroundCheckEnabled() in extension.ts)",
    );
  });

  test('"Check Project (javac)" refuses to run untrusted and never publishes javac diagnostics', async () => {
    const uri = fixtureUri("src", "main", "java", "demo", "TypeError.java");
    await vscode.workspace.openTextDocument(uri);

    const message = await captureErrorMessage(() =>
      vscode.commands.executeCommand("java-vsix-lite.checkProject"),
    );
    assert.ok(
      message && /untrusted workspace/i.test(message),
      `expected the untrusted-workspace refusal message from checkProject(), got: ${message}`,
    );

    // The command must have refused before spawning javac at all -- confirm
    // no diagnostic ever lands, not just that the error message appeared.
    const arrived = await waitFor(
      () =>
        vscode.languages
          .getDiagnostics(uri)
          .some((d) => d.source === "javac"),
      2_000,
    );
    assert.strictEqual(arrived, false, "Check Project must not have run javac untrusted");
  });

  test('"Install Dependencies" refuses to run untrusted (never spawns the build tool)', async () => {
    const message = await captureErrorMessage(() =>
      vscode.commands.executeCommand("java-vsix-lite.installDependencies"),
    );
    assert.ok(
      message && /untrusted workspace/i.test(message),
      `expected the untrusted-workspace refusal message from installDependencies(), got: ${message}`,
    );
  });

  test('"Download Missing Dependencies" refuses to run untrusted (no network I/O)', async () => {
    const message = await captureErrorMessage(() =>
      vscode.commands.executeCommand("java-vsix-lite.downloadDependencies"),
    );
    assert.ok(
      message && /untrusted workspace/i.test(message),
      `expected the untrusted-workspace refusal message from downloadDependencies(), got: ${message}`,
    );
  });

  test("debugging refuses to start untrusted (provider aborts before any process spawns)", async () => {
    const folder = vscode.workspace.workspaceFolders?.[0];
    let started: boolean | undefined;
    const message = await captureErrorMessage(async () => {
      started = await vscode.debug.startDebugging(folder, {
        type: "java-vsix-lite",
        name: "untrusted-launch",
        request: "launch",
        mainClass: "demo.Sample",
      });
    });
    assert.strictEqual(
      started,
      false,
      "startDebugging must resolve false untrusted -- resolveDebugConfiguration " +
        "returned undefined, aborting the session",
    );
    assert.ok(
      message && /untrusted workspace/i.test(message),
      `expected the untrusted-workspace refusal message from the debug provider, got: ${message}`,
    );
    assert.strictEqual(
      vscode.debug.activeDebugSession,
      undefined,
      "no debug session may exist after the untrusted refusal",
    );
  });
});
