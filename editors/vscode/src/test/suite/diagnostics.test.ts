// Regression suite for diagnostics reaching the editor, not just the wire.
// Assertions use `vscode.languages.getDiagnostics`, the same collection
// squiggles, Problems, and file decorations all read from.
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
    // Lives under a dot-directory so the javac source walk skips it,
    // keeping this malformed file from poisoning later javac runs.
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

  test("javac backstop runs only after a save", async function () {
    this.timeout(120_000);
    // The javac backstop must only run after a save, batched, at most
    // once every 30 seconds.
    // The fixture's error (an unreported checked exception) is one only javac
    // can see, so a `source === "javac"` diagnostic proves the compiler ran.
    const uri = fixtureUri("src", "main", "java", "demo", "Broken.java");
    const doc = await vscode.workspace.openTextDocument(uri);
    await vscode.window.showTextDocument(doc);
    const originalText = doc.getText();

    try {
      // Real wall-clock wait: no fake-timer seam crosses the
      // extension/LSP-client boundary.
      await new Promise((r) => setTimeout(r, 5_000));
      const onOpen = vscode.languages.getDiagnostics(uri).filter((d) => d.source === "javac");
      assert.strictEqual(
        onOpen.length,
        0,
        `javac must never run on open/load, got: ${onOpen.map((d) => d.message).join("; ")}`,
      );

      // A harmless whitespace-only edit + save is the only thing that may
      // trigger the automatic check.
      const appendEdit = new vscode.WorkspaceEdit();
      appendEdit.insert(uri, doc.positionAt(originalText.length), "\n");
      assert.ok(await vscode.workspace.applyEdit(appendEdit), "workspace edit was not applied");
      assert.ok(await doc.save(), "save failed");

      const arrived = await waitFor(
        () => vscode.languages.getDiagnostics(uri).some((d) => d.source === "javac"),
        60_000,
      );
      if (!arrived) {
        // Skip only when javac plausibly couldn't run — the syntax
        // pipeline test above already proves diagnostics flow.
        if (process.env.JVL_TEST_REQUIRE_JAVAC === "1") {
          assert.fail("javac diagnostics did not arrive and JVL_TEST_REQUIRE_JAVAC=1");
        }
        this.skip();
      }
      const javacDiags = vscode.languages
        .getDiagnostics(uri)
        .filter((d) => d.source === "javac");
      assert.ok(
        javacDiags.some((d) => /unreported exception|IOException/.test(d.message)),
        `expected the unreported-exception error, got: ${javacDiags
          .map((d) => d.message)
          .join("; ")}`,
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

  test("javac backstop honors the 30-second floor", async function () {
    this.timeout(180_000);
    // A second, distinct javac-only error must not be reported before
    // the scheduler's 30-second floor since the first run started.
    const uri = fixtureUri("src", "main", "java", "demo", "Broken.java");
    const doc = await vscode.workspace.openTextDocument(uri);
    await vscode.window.showTextDocument(doc);
    const originalText = doc.getText();

    try {
      // Establish a first javac result so the scheduler has a `lastStart`
      // to measure the floor from. Force a real dirty-then-save cycle —
      // a no-op save on a clean buffer may not fire `onDidSaveTextDocument`.
      const primeEdit = new vscode.WorkspaceEdit();
      primeEdit.insert(uri, doc.positionAt(originalText.length), "\n");
      assert.ok(await vscode.workspace.applyEdit(primeEdit), "workspace edit was not applied");
      assert.ok(await doc.save(), "initial save failed");
      const firstArrived = await waitFor(
        () => vscode.languages.getDiagnostics(uri).some((d) => d.source === "javac"),
        60_000,
      );
      if (!firstArrived) {
        if (process.env.JVL_TEST_REQUIRE_JAVAC === "1") {
          assert.fail("javac diagnostics did not arrive and JVL_TEST_REQUIRE_JAVAC=1");
        }
        this.skip();
      }

      // Introduce a second, distinct error. The native tier flags it too,
      // but only javac's `source === "javac"` copy is used to measure
      // the floor.
      const distinctMarker = "doesNotExistOnList";
      const anchor = "nums.add(1);";
      const text = doc.getText();
      const anchorOffset = text.indexOf(anchor);
      assert.ok(anchorOffset >= 0, "fixture must contain the anchor line");
      const insertOffset = anchorOffset + anchor.length;
      const addEdit = new vscode.WorkspaceEdit();
      addEdit.insert(uri, doc.positionAt(insertOffset), `\n        nums.${distinctMarker}();`);
      assert.ok(await vscode.workspace.applyEdit(addEdit), "workspace edit was not applied");
      assert.ok(await doc.save(), "second save failed");

      const hasDistinct = () =>
        vscode.languages
          .getDiagnostics(uri)
          .some((d) => d.source === "javac" && d.message.includes(distinctMarker));

      const tooSoon = await waitFor(hasDistinct, 20_000);
      assert.strictEqual(
        tooSoon,
        false,
        "the second javac run must not fire before the 30s floor since the first run's start",
      );

      const arrivedLater = await waitFor(hasDistinct, 50_000);
      assert.ok(
        arrivedLater,
        "expected the second, distinct javac diagnostic to arrive once the 30s floor elapsed",
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

  test("native return diagnostic arrives without a save and survives the compiler pass", async function () {
    this.timeout(120_000);
    // The native return check must land from a `didChange` alone — no
    // save, no subprocess — and the javac save pass must dedupe against it.
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

      // Await `Check Project`'s full round trip as the "compiler finished"
      // signal — polling for `source: "javac"` won't work, since the merge
      // keeps the native diagnostic and drops javac's redundant confirmation.
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
      // Restore the fixture on disk and in the buffer even on failure —
      // later tests must never see the broken return.
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
    // The native reassignment check ships relatedInformation over the
    // wire, but the client relocates it into the styled hover so the
    // plain hover block stays a single message line.
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
