// Probe suite for the VS Code plugin comparison baseline. Runs identically
// under both flavors and writes a normalized JSON report; it never throws on
// a slow or absent result -- a timeout is recorded as data, because the whole
// point is capturing what each plugin does.
const fs = require("fs");
const path = require("path");
const vscode = require("vscode");

const FLAVOR = process.env.JVL_COMPARE_FLAVOR;
const OUT = process.env.JVL_COMPARE_OUT;
const PROJECT = process.env.JVL_COMPARE_PROJECT;
const READY_TIMEOUT_MS = 300_000;
const PROBE_TIMEOUT_MS = 120_000;
const BASELINE_TIMEOUT_MS = 5_000;
// How long a server must stay quiet, after publishing something, before we
// treat its answer as final. This is the ceiling on wasted waiting, so it
// wants to be small — but it must clear the slowest observed edit-to-publish
// latency with margin, which was redhat.java's ~2.1s cross-file rename.
const SETTLE_QUIET_MS = 6_000;
const POLL_MS = 100;

const EXTENSION_ID_BY_FLAVOR = {
  none: null,
  ours: "java-vsix-lite.java-vsix-lite",
  redhat: "redhat.java",
};
const EXTENSION_ID = EXTENSION_ID_BY_FLAVOR[FLAVOR] ?? null;
// With no extension there is nothing to become ready and nothing that will
// ever publish a diagnostic, so the baseline uses short windows: it should
// record "nothing happened" quickly rather than idling out the full
// plugin-sized timeouts.
const BASELINE = EXTENSION_ID === null;

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

/** Poll `predicate` until truthy or `timeoutMs` elapses; returns the value or undefined. */
async function waitFor(predicate, timeoutMs) {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    const value = await predicate();
    if (value !== undefined && value !== null && value !== false) {
      return value;
    }
    if (Date.now() >= deadline) {
      return undefined;
    }
    await sleep(500);
  }
}

/** Absolute paths differ per run/container; collapse them to a stable token. */
function normalizePath(value) {
  return String(value).split(PROJECT).join("${PROJECT}");
}

function uriKey(uri) {
  return normalizePath(uri.fsPath);
}

function fixtureUri(...segments) {
  return vscode.Uri.file(path.join(PROJECT, ...segments));
}

/** Position of the `occurrence`-th (1-based) appearance of `token`. */
function positionOf(document, token, occurrence = 1) {
  const text = document.getText();
  let index = -1;
  for (let n = 0; n < occurrence; n++) {
    index = text.indexOf(token, index + 1);
    if (index < 0) {
      throw new Error(`token ${token} #${occurrence} not found`);
    }
  }
  return document.positionAt(index);
}

function normalizeDiagnostics(uri) {
  return vscode.languages
    .getDiagnostics(uri)
    .map((d) => ({
      severity: vscode.DiagnosticSeverity[d.severity],
      source: d.source ?? null,
      code:
        d.code && typeof d.code === "object" ? String(d.code.value) : d.code ?? null,
      message: d.message,
      line: d.range.start.line,
      character: d.range.start.character,
    }))
    .sort(
      (a, b) =>
        a.line - b.line ||
        a.character - b.character ||
        String(a.code).localeCompare(String(b.code)) ||
        a.message.localeCompare(b.message),
    );
}

function normalizeHover(hovers) {
  return (hovers ?? [])
    .flatMap((hover) => hover.contents)
    .map((content) =>
      typeof content === "string" ? content : content.value ?? String(content),
    )
    .map((value) => value.trim())
    .filter((value) => value.length > 0);
}

function normalizeLocations(locations) {
  return (locations ?? [])
    .map((location) => {
      const uri = location.uri ?? location.targetUri;
      const range = location.range ?? location.targetSelectionRange;
      return `${uriKey(uri)}#L${range.start.line + 1}:${range.start.character + 1}`;
    })
    .sort();
}

async function probe(report, id, kind, body) {
  const started = Date.now();
  let value = null;
  let timedOut = false;
  let error = null;
  try {
    value = await body();
    if (value === undefined) {
      timedOut = true;
      value = null;
    }
  } catch (err) {
    error = String((err && err.message) || err);
  }
  report.probes.push({
    id,
    kind,
    value,
    timedOut,
    error,
    startedAt: started,
    endedAt: Date.now(),
    elapsedMs: Date.now() - started,
  });
}

async function run() {
  const report = {
    flavor: FLAVOR,
    extensionId: EXTENSION_ID,
    extensionVersion: null,
    vscodeVersion: vscode.version,
    // Seconds of quiet time runFlavor.js waited after installing the
    // extension before it started sampling and launched this suite.
    settleSeconds: Number(process.env.JVL_COMPARE_SETTLE_SECONDS ?? 0),
    flavorStartedAt: Date.now(),
    probes: [],
  };

  // The baseline flavor deliberately has no Java extension: it runs the
  // exact same document opens, edits, and provider requests so its report
  // and resource trace show what the container, VS Code, and this suite
  // cost with zero language support. Every probe below simply comes back
  // empty for it, which is the point.
  const extension = EXTENSION_ID ? vscode.extensions.getExtension(EXTENSION_ID) : null;
  if (EXTENSION_ID && !extension) {
    report.probes.push({
      id: "extension.present",
      kind: "fatal",
      value: false,
      timedOut: false,
      error: `${EXTENSION_ID} is not installed`,
      elapsedMs: 0,
    });
    report.flavorEndedAt = Date.now();
    fs.writeFileSync(OUT, JSON.stringify(report, null, 2));
    return;
  }
  let api = null;
  if (extension) {
    report.extensionVersion = extension.packageJSON.version;
    api = await extension.activate();
  }

  const ordersUri = fixtureUri("src", "main", "java", "demo", "Orders.java");
  const mainUri = fixtureUri("src", "main", "java", "demo", "Main.java");
  const ordersDoc = await vscode.workspace.openTextDocument(ordersUri);
  await vscode.window.showTextDocument(ordersDoc);
  const mainDoc = await vscode.workspace.openTextDocument(mainUri);

  // Readiness: redhat.java exposes serverReady(); ours is ready once document
  // symbols come back non-empty. The baseline has neither, so it just
  // confirms quickly that nothing answers.
  const readyTimeout = BASELINE ? BASELINE_TIMEOUT_MS : READY_TIMEOUT_MS;
  const readyStarted = Date.now();
  if (api && typeof api.serverReady === "function") {
    await Promise.race([api.serverReady(), sleep(readyTimeout)]);
  }
  const symbolsReady = await waitFor(async () => {
    const symbols = await vscode.commands.executeCommand(
      "vscode.executeDocumentSymbolProvider",
      ordersUri,
    );
    return symbols && symbols.length > 0 ? symbols : undefined;
  }, readyTimeout);
  report.probes.push({
    id: "server.ready",
    kind: "readiness",
    value: Boolean(symbolsReady),
    timedOut: !symbolsReady,
    error: null,
    startedAt: readyStarted,
    endedAt: Date.now(),
    elapsedMs: Date.now() - readyStarted,
  });

  await probe(report, "diagnostics.clean", "diagnostics", async () =>
    normalizeDiagnostics(ordersUri),
  );

  await probe(report, "symbols.orders", "symbols", async () => {
    const symbols =
      (await vscode.commands.executeCommand(
        "vscode.executeDocumentSymbolProvider",
        ordersUri,
      )) ?? [];
    const names = [];
    const walk = (nodes) => {
      for (const node of nodes) {
        names.push(`${vscode.SymbolKind[node.kind]} ${node.name}`);
        walk(node.children ?? []);
      }
    };
    walk(symbols);
    return names.sort();
  });

  await probe(report, "hover.total", "hover", async () => {
    const position = positionOf(mainDoc, "total()");
    return normalizeHover(
      await vscode.commands.executeCommand(
        "vscode.executeHoverProvider",
        mainUri,
        position,
      ),
    );
  });

  await probe(report, "definition.total", "definition", async () => {
    const position = positionOf(mainDoc, "total()");
    return normalizeLocations(
      await vscode.commands.executeCommand(
        "vscode.executeDefinitionProvider",
        mainUri,
        position,
      ),
    );
  });

  await probe(report, "completion.orders", "completion", async () => {
    const anchor = positionOf(mainDoc, "orders.total()");
    const position = anchor.translate(0, "orders.".length);
    const list = await vscode.commands.executeCommand(
      "vscode.executeCompletionItemProvider",
      mainUri,
      position,
    );
    return (list?.items ?? [])
      .map((item) =>
        typeof item.label === "string" ? item.label : item.label.label,
      )
      .sort()
      .slice(0, 20);
  });

  // ---- Scripted edit workload -------------------------------------------
  //
  // Each step makes one realistic change and waits for the language server
  // to settle into the expected state, so the timings say "how long until
  // this plugin agreed with reality", not just "how long until it said
  // something". They escalate deliberately: a local type error, a bad
  // member on a project type, a dependency (Guava) misuse, a deleted import,
  // a cross-file signature change that only breaks *another* file, a purely
  // additive valid edit that must stay clean, then a full revert.
  const originals = new Map([
    [ordersUri.fsPath, ordersDoc.getText()],
    [mainUri.fsPath, mainDoc.getText()],
  ]);
  const editTimeout = BASELINE ? BASELINE_TIMEOUT_MS : PROBE_TIMEOUT_MS;

  /** Replace the first occurrence of `find` in `uri` with `replace`, and save. */
  async function applyEdit(uri, find, replace) {
    const doc = await vscode.workspace.openTextDocument(uri);
    const text = doc.getText();
    const at = text.indexOf(find);
    if (at < 0) {
      throw new Error(`edit anchor not found in ${path.basename(uri.fsPath)}: ${find}`);
    }
    const edit = new vscode.WorkspaceEdit();
    edit.replace(uri, new vscode.Range(doc.positionAt(at), doc.positionAt(at + find.length)), replace);
    await vscode.workspace.applyEdit(edit);
    await doc.save();
  }

  /** Restore one file to its original text, and save. */
  async function restore(uri) {
    const doc = await vscode.workspace.openTextDocument(uri);
    const edit = new vscode.WorkspaceEdit();
    edit.replace(
      uri,
      new vscode.Range(doc.positionAt(0), doc.positionAt(doc.getText().length)),
      originals.get(uri.fsPath),
    );
    await vscode.workspace.applyEdit(edit);
    await doc.save();
  }

  /** Wait until both fixture files report nothing, so edits can't leak state. */
  async function waitForClean() {
    return await waitFor(async () => {
      const all = [...normalizeDiagnostics(ordersUri), ...normalizeDiagnostics(mainUri)];
      return all.length === 0 ? true : undefined;
    }, editTimeout);
  }

  /**
   * Apply one edit and wait for the server's *verdict* on it.
   *
   * Matching on `expect` rather than "any diagnostic appeared" is
   * load-bearing. A stale diagnostic left over from the previous edit — or a
   * generic banner like jdt.ls's "X.java is a non-project file, only syntax
   * errors are reported" — satisfies an any-diagnostic test instantly, which
   * silently turns "this plugin never analyzed anything" into a row of
   * impressively fast timings. Both failure modes were observed while
   * building this harness.
   *
   * The wait watches publications rather than the clock. Once a server has
   * published for these files after the edit and then gone quiet, its answer
   * is final and waiting longer cannot change it. That distinction matters:
   * "published, but never mentioned the thing we broke" is a capability
   * result worth recording in seconds, whereas burning the full ceiling to
   * reach the same conclusion costs minutes and throws away the payload the
   * plugin did send. Only genuine silence — no publication at all — runs out
   * the clock.
   */
  async function waitForVerdict(watched, edited, expect) {
    const active = [watched.toString(), edited.toString()];
    let lastPublishAt = 0;
    const subscription = vscode.languages.onDidChangeDiagnostics((event) => {
      if (event.uris.some((uri) => active.includes(uri.toString()))) {
        lastPublishAt = Date.now();
      }
    });
    try {
      const deadline = Date.now() + editTimeout;
      for (;;) {
        const diagnostics = normalizeDiagnostics(watched);
        const matched = diagnostics.filter(
          (d) => d.severity === "Error" && expect.some((needle) => d.message.includes(needle)),
        );
        if (matched.length > 0) {
          return { outcome: "matched", diagnostics };
        }
        // Quiet only counts once something was actually published after the
        // edit; a server that has said nothing at all is still thinking.
        if (lastPublishAt > 0 && Date.now() - lastPublishAt > SETTLE_QUIET_MS) {
          return { outcome: "answered-without-match", diagnostics };
        }
        if (Date.now() >= deadline) {
          return undefined;
        }
        await sleep(POLL_MS);
      }
    } finally {
      subscription.dispose();
    }
  }

  /**
   * Wait until the servers watching `uris` have published something and then
   * gone quiet, so "it reported nothing" means "it looked and had nothing to
   * say" rather than "we asked before it started". Bounded: a server that
   * never publishes is reported as such instead of consuming the ceiling.
   */
  async function waitForSettle(uris) {
    const active = uris.map((uri) => uri.toString());
    let lastPublishAt = 0;
    const subscription = vscode.languages.onDidChangeDiagnostics((event) => {
      if (event.uris.some((uri) => active.includes(uri.toString()))) {
        lastPublishAt = Date.now();
      }
    });
    try {
      const deadline = Date.now() + Math.min(editTimeout, SETTLE_QUIET_MS * 2);
      for (;;) {
        if (lastPublishAt > 0 && Date.now() - lastPublishAt > SETTLE_QUIET_MS) {
          return true;
        }
        if (Date.now() >= deadline) {
          return false;
        }
        await sleep(POLL_MS);
      }
    } finally {
      subscription.dispose();
    }
  }


  async function editProbe(id, { uri, find, replace, watch, expect, restoreFirst }) {
    await probe(report, id, "edit", async () => {
      if (restoreFirst) {
        for (const target of restoreFirst) {
          await restore(target);
        }
        await waitForClean();
      }
      await applyEdit(uri, find, replace);
      return await waitForVerdict(watch ?? uri, uri, expect);
    });
  }

  // Each `expect` lists substrings that a correct diagnostic for *this* edit
  // would contain; wording differs per plugin ("cannot be converted to" vs
  // "Type mismatch"), so the symbol we broke is the stable anchor.

  // 1. Local type error: `int sum` can't hold a String.
  await editProbe("edit.localTypeError", {
    uri: ordersUri,
    find: "int sum = 0;",
    replace: "String sum = 0;",
    expect: ["String", "int"],
  });

  // 2. Unknown member on a project type: Order has no `price()`.
  await editProbe("edit.unknownMember", {
    uri: ordersUri,
    find: "order.lineTotal().minorUnits()",
    replace: "order.price().minorUnits()",
    expect: ["price"],
    restoreFirst: [ordersUri],
  });

  // 3. Dependency misuse: Guava's ImmutableList has no `copyOfRange`.
  //    Only resolvable with the dependency jar actually on the classpath.
  await editProbe("edit.dependencyMisuse", {
    uri: ordersUri,
    find: "ImmutableList.copyOf(items)",
    replace: "ImmutableList.copyOfRange(items)",
    expect: ["copyOfRange"],
    restoreFirst: [ordersUri],
  });

  // 4. Deleted import: every `ImmutableList` reference goes unresolved.
  await editProbe("edit.removedImport", {
    uri: ordersUri,
    find: "import com.google.common.collect.ImmutableList;",
    replace: "",
    expect: ["ImmutableList"],
    restoreFirst: [ordersUri],
  });

  // 5. Cross-file break: renaming `total()` in Orders.java leaves Main.java
  //    calling a method that no longer exists. The error must surface in the
  //    *other* file — the one that wasn't edited.
  await editProbe("edit.crossFileRename", {
    uri: ordersUri,
    find: "public int total() {",
    replace: "public int totalRenamed() {",
    watch: mainUri,
    expect: ["total"],
    restoreFirst: [ordersUri, mainUri],
  });

  // 6. Additive, entirely valid edit: a new method built from existing API.
  //    Nothing may be reported — this catches false positives and measures
  //    how quickly a plugin re-analyzes after a benign change.
  await probe(report, "edit.validAddition", "edit", async () => {
    await restore(ordersUri);
    await restore(mainUri);
    await waitForClean();
    await applyEdit(
      ordersUri,
      "    public int count() {",
      "    public Money averageLine() {\n" +
        "        return count() == 0 ? Money.zero(currency) : totalMoney();\n" +
        "    }\n\n" +
        "    public int count() {",
    );
    // "Stayed clean" is only meaningful if the server actually looked: wait
    // for it to publish and settle, then read the result. Sleeping a fixed
    // interval instead would credit a plugin that never analyzed at all.
    const settled = await waitForSettle([ordersUri, mainUri]);
    const diagnostics = [...normalizeDiagnostics(ordersUri), ...normalizeDiagnostics(mainUri)];
    return {
      outcome: settled ? (diagnostics.length === 0 ? "clean" : "reported") : "no-response",
      diagnostics,
    };
  });

  // 7. Full revert: everything back to the committed fixture, all clear.
  await probe(report, "edit.revertAll", "edit", async () => {
    await restore(ordersUri);
    await restore(mainUri);
    const clean = await waitForClean();
    const diagnostics = [...normalizeDiagnostics(ordersUri), ...normalizeDiagnostics(mainUri)];
    return { outcome: clean ? "clean" : "no-response", diagnostics };
  });

  // Always restore the fixture on disk so a re-run starts from a clean tree,
  // even if a probe above threw or timed out mid-edit.
  for (const [file, text] of originals) {
    fs.writeFileSync(file, text);
  }
  fs.mkdirSync(path.dirname(OUT), { recursive: true });
  report.flavorEndedAt = Date.now();
  fs.writeFileSync(OUT, JSON.stringify(report, null, 2));
}

module.exports = { run };
