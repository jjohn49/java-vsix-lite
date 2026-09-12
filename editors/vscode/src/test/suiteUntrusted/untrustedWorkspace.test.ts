// Verifies pure-Rust features stay active while every trust-gated feature refuses.
// `suiteSetup` overrides VS Code's always-trusted Extension Host signal for this suite.
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
 * Intercepts `vscode.window.showErrorMessage` to capture a trust-gated
 * command's refusal text without a real modal appearing.
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
    // Force the untrusted signal (see file header). Fail loudly if a future
    // VS Code makes this property non-configurable -- every assertion below
    // depends on it.
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
    const originalText = doc.getText();

    try {
      // Only a save can trigger the automatic javac backstop, so trigger a
      // real save to prove the trust gate actually blocks it.
      const appendEdit = new vscode.WorkspaceEdit();
      appendEdit.insert(uri, doc.positionAt(originalText.length), "\n");
      assert.ok(await vscode.workspace.applyEdit(appendEdit), "workspace edit was not applied");
      assert.ok(await doc.save(), "save failed");

      // Wait past the 1.5s debounce so the javac check would fire here if
      // the trust gate were broken.
      await new Promise((r) => setTimeout(r, 5_000));
      const javacDiags = vscode.languages.getDiagnostics(uri).filter((d) => d.source === "javac");
      assert.strictEqual(
        javacDiags.length,
        0,
        "javac diagnostics appeared in an untrusted workspace -- checkOnSave must " +
          "be trust-gated (see javacBackgroundCheckEnabled() in extension.ts)",
      );
    } finally {
      const restoreEdit = new vscode.WorkspaceEdit();
      restoreEdit.replace(
        uri,
        new vscode.Range(doc.positionAt(0), doc.positionAt(doc.getText().length)),
        originalText,
      );
      await vscode.workspace.applyEdit(restoreEdit);
      await doc.save();
    }
  });

  test("native return diagnostic is not trust-gated: jvl.incompatibleReturn arrives with no javac", async () => {
    // The native return-type diagnostic works in an untrusted workspace with
    // no JDK process, Workspace Trust, or compiler setting involved.
    const uri = fixtureUri("src", "main", "java", "demo", "ReturnTypeError.java");
    const doc = await vscode.workspace.openTextDocument(uri);
    await vscode.window.showTextDocument(doc);

    const arrived = await waitFor(
      () =>
        vscode.languages
          .getDiagnostics(uri)
          .some((d) => d.code === "jvl.incompatibleReturn"),
      30_000,
    );
    assert.ok(
      arrived,
      "expected the native jvl.incompatibleReturn diagnostic in an untrusted " +
        "workspace -- the default-tier return check must not be trust-gated",
    );
    const native = vscode.languages
      .getDiagnostics(uri)
      .filter((d) => d.code === "jvl.incompatibleReturn");
    assert.strictEqual(native.length, 1, "expected exactly one native return diagnostic");
    assert.strictEqual(native[0].source, "java-vsix-lite");
    assert.strictEqual(
      native[0].message,
      "incompatible types: String cannot be converted to int",
    );

    // Confirm the native diagnostic didn't come from a leaked javac run.
    const javacDiags = vscode.languages.getDiagnostics(uri).filter((d) => d.source === "javac");
    assert.strictEqual(
      javacDiags.length,
      0,
      "no diagnostic may have source === \"javac\" in an untrusted workspace",
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

    // Confirm javac never actually ran, not just that the error message
    // appeared.
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

  test("test explorer run refuses untrusted and starts no debug session", async () => {
    const ext = vscode.extensions.getExtension("java-vsix-lite.java-vsix-lite");
    assert.ok(ext, "extension not found under id java-vsix-lite.java-vsix-lite");
    const api = (await ext!.activate()) as {
      testing?: { runProfile: vscode.TestRunProfile };
    };
    assert.ok(api?.testing, "activate() must export the testing API");

    const tokenSource = new vscode.CancellationTokenSource();
    const message = await captureErrorMessage(async () => {
      await api.testing!.runProfile.runHandler!(
        new vscode.TestRunRequest(),
        tokenSource.token,
      );
    });
    assert.strictEqual(
      message,
      "java-vsix-lite: running tests is disabled in an untrusted workspace — it runs your project's code. Trust this workspace to enable it.",
      "the test-run trust gate must show its exact refusal message",
    );
    assert.strictEqual(
      vscode.debug.activeDebugSession,
      undefined,
      "no debug session may exist after the untrusted test-run refusal",
    );
  });
});
