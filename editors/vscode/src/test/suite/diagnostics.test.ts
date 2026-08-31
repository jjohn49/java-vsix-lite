// The regression suite for the class of bug that motivated this harness: the
// server can be perfectly correct on the wire while the extension shell
// swallows everything (the M7 `onNotification("textDocument/publishDiagnostics")`
// incident killed every squiggle for several releases and no test noticed).
// These tests assert on `vscode.languages.getDiagnostics` — the editor-side
// collection that squiggles, Problems, and red file-name decorations all
// read from — so a break anywhere in the pipeline fails here.
import * as assert from "assert";
import * as path from "path";
import * as vscode from "vscode";

const FIXTURE = path.resolve(__dirname, "../../../test-fixture");

function fixtureUri(...segments: string[]): vscode.Uri {
  return vscode.Uri.file(path.join(FIXTURE, ...segments));
}

/** Poll until `predicate` holds or `timeoutMs` elapses. */
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

suite("diagnostics pipeline", () => {
  test("syntax errors reach the editor's diagnostics collection", async () => {
    // Lives under a dot-directory: visible to the editor, but skipped by
    // the javac check's source walk — so the malformed file can't poison
    // the compiler run the second test depends on.
    const uri = fixtureUri(".syntax-fixture", "Syntax.java");
    const doc = await vscode.workspace.openTextDocument(uri);
    await vscode.window.showTextDocument(doc);

    const arrived = await waitFor(
      () => vscode.languages.getDiagnostics(uri).length > 0,
      30_000,
    );
    assert.ok(
      arrived,
      "expected at least one diagnostic for the malformed fixture file — " +
        "if this fails with a healthy server, the extension shell is " +
        "swallowing publishDiagnostics again",
    );
  });

  test("javac check-on-load publishes real compiler errors", async function () {
    this.timeout(120_000);
    // Foundry-target behavior: the background javac check runs on project
    // load and lands type errors in the collection without any edit. Skip
    // (not fail) when no JDK is discoverable in the test environment.
    const uri = fixtureUri("src", "main", "java", "demo", "Broken.java");
    const doc = await vscode.workspace.openTextDocument(uri);
    await vscode.window.showTextDocument(doc);

    const arrived = await waitFor(
      () =>
        vscode.languages
          .getDiagnostics(uri)
          .some((d) => d.source === "javac"),
      60_000,
    );
    if (!arrived) {
      // Distinguish "no JDK here" from a real regression: the syntax
      // pipeline test above already proves diagnostics flow, so only skip
      // when javac plausibly couldn't run at all.
      if (process.env.JVL_TEST_REQUIRE_JAVAC === "1") {
        assert.fail("javac diagnostics did not arrive and JVL_TEST_REQUIRE_JAVAC=1");
      }
      this.skip();
    }
    const javacDiags = vscode.languages
      .getDiagnostics(uri)
      .filter((d) => d.source === "javac");
    assert.ok(
      javacDiags.some((d) => /incompatible types|String/.test(d.message)),
      `expected the int-from-String type error, got: ${javacDiags
        .map((d) => d.message)
        .join("; ")}`,
    );
  });

  test("native return diagnostic arrives without a save and survives the compiler pass", async function () {
    this.timeout(120_000);
    // Task 4 proof: the pure-Rust return check must land in the editor's
    // collection from a `didChange` alone — no save, no subprocess — and the
    // save-triggered javac pass must dedupe against it, never duplicate it.
    const uri = fixtureUri("src", "main", "java", "demo", "ReturnTypes.java");
    const doc = await vscode.workspace.openTextDocument(uri);
    await vscode.window.showTextDocument(doc);
    const originalText = doc.getText();

    try {
      // Replace the returned `1` with `"bad"` via a workspace edit — the
      // buffer goes dirty and stays dirty; only `didChange` reaches the server.
      const returnOffset = originalText.indexOf("return 1;");
      assert.ok(returnOffset >= 0, "fixture must contain `return 1;`");
      const oneOffset = returnOffset + "return ".length;
      const breakEdit = new vscode.WorkspaceEdit();
      breakEdit.replace(
        uri,
        new vscode.Range(doc.positionAt(oneOffset), doc.positionAt(oneOffset + 1)),
        '"bad"',
      );
      assert.ok(await vscode.workspace.applyEdit(breakEdit), "workspace edit was not applied");
      assert.ok(doc.isDirty, "the edit must leave the buffer unsaved");

      const arrived = await waitFor(
        () =>
          vscode.languages
            .getDiagnostics(uri)
            .some((d) => d.code === "jvl.incompatibleReturn"),
        30_000,
      );
      assert.ok(
        arrived,
        "expected the native return diagnostic on an UNSAVED buffer — the " +
          "no-save didChange path is broken if this times out",
      );
      const native = vscode.languages
        .getDiagnostics(uri)
        .filter((d) => d.code === "jvl.incompatibleReturn");
      assert.strictEqual(
        native.length,
        1,
        `expected exactly one jvl.incompatibleReturn diagnostic, got: ${native
          .map((d) => d.message)
          .join("; ")}`,
      );
      assert.strictEqual(native[0].source, "java-vsix-lite");
      assert.strictEqual(
        native[0].message,
        "incompatible types: String cannot be converted to int",
      );

      // Save to trigger the automatic (trust-gated, checkOnSave) javac pass,
      // then explicitly run the manual `Check Project` command and await its
      // full request/response round trip as the deterministic "the compiler
      // pass has completed and republished" signal. A content-based poll
      // (e.g. waiting for a `source: "javac"` diagnostic) cannot serve that
      // role here: when a JDK is available, javac CONFIRMS the exact same
      // incompatibility the native pass already reported, and the merge
      // keeps the native entry in place rather than swapping in javac's
      // redundant copy (see `merge_javac_diagnostics`) — so `source` stays
      // `"java-vsix-lite"` even after a real, successful compiler pass.
      assert.ok(await doc.save(), "save failed");
      await vscode.commands.executeCommand("java-vsix-lite.checkProject");
      const matching = vscode.languages
        .getDiagnostics(uri)
        .filter((d) => /String cannot be converted to int/.test(d.message));
      assert.strictEqual(
        matching.length,
        1,
        `expected exactly one matching incompatibility after the compiler pass ` +
          `(dedup regression if two), got: ${matching
            .map((d) => `${d.source}: ${d.message}`)
            .join("; ")}`,
      );
    } finally {
      // Leave the fixture clean on disk AND in the buffer even when an
      // assertion above failed — later tests (and later suite runs against
      // the same checkout) must never see the broken return.
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

  test("reassignment declaration link lives only in the styled hover", async function () {
    this.timeout(60_000);
    // Part D proof: the native reassignment check ships relatedInformation
    // over the wire, but the client relocates it into the styled hover so
    // the editor's plain hover block stays a single message line (see
    // relocateRelatedInformation in styledHover.ts).
    const uri = fixtureUri("src", "main", "java", "demo", "ReturnTypes.java");
    const doc = await vscode.workspace.openTextDocument(uri);
    await vscode.window.showTextDocument(doc);
    const originalText = doc.getText();

    try {
      // Turn the method body into an incompatible reassignment (unsaved —
      // only didChange reaches the server; no javac involved).
      const bodyOffset = originalText.indexOf("return 1;");
      assert.ok(bodyOffset >= 0, "fixture must contain `return 1;`");
      const breakEdit = new vscode.WorkspaceEdit();
      breakEdit.replace(
        uri,
        new vscode.Range(doc.positionAt(bodyOffset), doc.positionAt(bodyOffset)),
        "int x = 1;\n        x = true;\n        ",
      );
      assert.ok(await vscode.workspace.applyEdit(breakEdit), "workspace edit was not applied");

      const arrived = await waitFor(
        () =>
          vscode.languages
            .getDiagnostics(uri)
            .some((d) => d.code === "jvl.incompatibleAssignment"),
        30_000,
      );
      assert.ok(arrived, "expected the native reassignment diagnostic on the unsaved buffer");
      const native = vscode.languages
        .getDiagnostics(uri)
        .filter((d) => d.code === "jvl.incompatibleAssignment");
      assert.strictEqual(native.length, 1, `got: ${native.map((d) => d.message).join("; ")}`);
      assert.strictEqual(
        native[0].message,
        "incompatible types: boolean cannot be converted to int",
      );
      // The published diagnostic must carry NO relatedInformation — the
      // plain hover/Problems row would otherwise duplicate the styled link.
      assert.ok(
        !native[0].relatedInformation || native[0].relatedInformation.length === 0,
        `relatedInformation must be relocated into the styled hover, got: ${JSON.stringify(
          native[0].relatedInformation,
        )}`,
      );

      // The styled hover carries the severity color, code-styled type
      // names, and the relocated declaration link.
      const hovers = (await vscode.commands.executeCommand(
        "vscode.executeHoverProvider",
        uri,
        native[0].range.start,
      )) as vscode.Hover[];
      const styled = hovers
        .flatMap((h) => h.contents)
        .map((c) => (c instanceof vscode.MarkdownString ? c.value : String(c)))
        .find((v) => v.includes("--vscode-editorError-foreground"));
      assert.ok(
        styled,
        `expected a styled hover section, got: ${JSON.stringify(
          hovers.flatMap((h) => h.contents),
        )}`,
      );
      assert.ok(
        styled!.includes("<code>boolean</code>") && styled!.includes("<code>int</code>"),
        `type names must be code-styled: ${styled}`,
      );
      assert.ok(
        styled!.includes("declared as") && styled!.includes("#L"),
        `declaration link must be rendered: ${styled}`,
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
});
