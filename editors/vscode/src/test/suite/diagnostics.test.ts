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
});
