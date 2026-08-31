// JUnit test support: Test Explorer discovery, run, and debug.
//
// Discovery is fully static and safe-tier: the server's `jvl/tests` request
// classifies JUnit 4/5 annotations syntactically (no execution, no
// classpath). Running tests EXECUTES project code, so both run profiles are
// Workspace Trust-gated exactly like debugging and `javac`. Execution goes
// through the existing DAP adapter (`jvl-server dap`) launching the JUnit
// Platform Console Launcher — never `mvn test`/`gradle test`, per the threat
// model's ban on default-tier build-file execution. Results are parsed from
// the launcher's `--reports-dir` legacy JUnit XML.

import * as fs from "fs";
import * as os from "os";
import * as path from "path";
import * as vscode from "vscode";
import { LanguageClient, State } from "vscode-languageclient/node";

import * as mavenFetch from "./mavenFetch";

/** Default JUnit Platform version for the console launcher. The 1.x line
 * keeps the Java 8+ baseline (JUnit 6 requires Java 17). */
const DEFAULT_LAUNCHER_VERSION = "1.13.4";

/** Same message pattern as the extension's other trust gates. */
const UNTRUSTED_MESSAGE =
  "java-vsix-lite: running tests is disabled in an untrusted workspace — it runs your project's code. Trust this workspace to enable it.";

/** Re-query debounce for edits to open test files (mirrors the save-check
 * debounce pattern in extension.ts, shorter because discovery is cheap). */
const DISCOVERY_DEBOUNCE_MS = 500;

const TEST_FILE_GLOB = "**/src/test/java/**/*.java";
const TEST_FILE_EXCLUDE = "**/{target,build,node_modules}/**";

// ---------------------------------------------------------------------------
// `jvl/tests` response shapes (crates/server/src/main.rs).
// ---------------------------------------------------------------------------

interface ServerRange {
  start: { line: number; character: number };
  end: { line: number; character: number };
}

interface ServerTestMethod {
  name: string;
  range: ServerRange;
  kind: "junit5" | "junit4";
}

interface ServerTestClass {
  fqn: string;
  name: string;
  range: ServerRange;
  methods: ServerTestMethod[];
}

interface ServerTestsFile {
  uri: string;
  classes: ServerTestClass[];
}

interface ServerTestsResult {
  files: ServerTestsFile[];
}

/** What a TestItem stands for — drives selector construction and XML mapping. */
type ItemData =
  | { type: "file" }
  | { type: "class"; fqn: string }
  | { type: "method"; fqn: string; method: string };

const itemData = new WeakMap<vscode.TestItem, ItemData>();

function toRange(range: ServerRange): vscode.Range {
  return new vscode.Range(
    range.start.line,
    range.start.character,
    range.end.line,
    range.end.character,
  );
}

/** The Test Explorer surface `activate()` exports — the Electron test
 * suites drive the run profiles programmatically through this. */
export interface TestingApi {
  controller: vscode.TestController;
  runProfile: vscode.TestRunProfile;
  debugProfile: vscode.TestRunProfile;
}

/**
 * Wire up the Test Explorer: controller, static discovery, and the two
 * trust-gated run profiles. `getClient` is read lazily on every request so
 * server restarts are transparent.
 */
export function activateTesting(
  context: vscode.ExtensionContext,
  getClient: () => LanguageClient | undefined,
): TestingApi {
  const controller = vscode.tests.createTestController(
    "java-vsix-lite",
    "Java Tests (java-vsix-lite)",
  );
  context.subscriptions.push(controller);

  const runningClient = (): LanguageClient | undefined => {
    const client = getClient();
    return client && client.state === State.Running ? client : undefined;
  };

  /** Query `jvl/tests` for `uris` and rebuild their item subtrees. */
  async function queryFiles(uris: string[]): Promise<void> {
    const client = runningClient();
    if (!client || uris.length === 0) {
      return; // silent no-op — items refresh on the next explicit refresh
    }
    let response: ServerTestsResult;
    try {
      response = await client.sendRequest("jvl/tests", { uris });
    } catch {
      return; // server stopping mid-request; next refresh recovers
    }
    for (const file of response.files) {
      applyFile(file);
    }
  }

  function applyFile(file: ServerTestsFile): void {
    if (file.classes.length === 0) {
      controller.items.delete(file.uri);
      return;
    }
    const uri = vscode.Uri.parse(file.uri);
    const fileItem =
      controller.items.get(file.uri) ??
      controller.createTestItem(file.uri, path.basename(uri.fsPath), uri);
    itemData.set(fileItem, { type: "file" });
    const classItems: vscode.TestItem[] = [];
    for (const cls of file.classes) {
      const classId = `${file.uri}#${cls.fqn}`;
      const classItem = controller.createTestItem(classId, cls.name, uri);
      classItem.range = toRange(cls.range);
      itemData.set(classItem, { type: "class", fqn: cls.fqn });
      const methodItems: vscode.TestItem[] = [];
      for (const method of cls.methods) {
        const methodItem = controller.createTestItem(
          `${classId}#${method.name}`,
          method.name,
          uri,
        );
        methodItem.range = toRange(method.range);
        itemData.set(methodItem, { type: "method", fqn: cls.fqn, method: method.name });
        methodItems.push(methodItem);
      }
      classItem.children.replace(methodItems);
      classItems.push(classItem);
    }
    fileItem.children.replace(classItems);
    controller.items.add(fileItem);
  }

  async function discoverAll(): Promise<void> {
    if (!runningClient()) {
      return;
    }
    const files = await vscode.workspace.findFiles(TEST_FILE_GLOB, TEST_FILE_EXCLUDE);
    await queryFiles(files.map((f) => f.toString()));
  }

  controller.resolveHandler = async (item) => {
    if (!item) {
      await discoverAll();
    }
  };
  controller.refreshHandler = async () => {
    await discoverAll();
  };

  // File events: re-query changed/created test files, drop removed ones.
  const watcher = vscode.workspace.createFileSystemWatcher(TEST_FILE_GLOB);
  context.subscriptions.push(watcher);
  watcher.onDidCreate((uri) => void queryFiles([uri.toString()]));
  watcher.onDidChange((uri) => void queryFiles([uri.toString()]));
  watcher.onDidDelete((uri) => controller.items.delete(uri.toString()));

  // Live edits to open test files: debounced re-query (same coalescing idea
  // as extension.ts's SAVE_CHECK_DEBOUNCE_MS pattern).
  const pendingUris = new Set<string>();
  let debounceTimer: ReturnType<typeof setTimeout> | undefined;
  context.subscriptions.push(
    vscode.workspace.onDidChangeTextDocument((event) => {
      const doc = event.document;
      if (doc.languageId !== "java" || doc.uri.scheme !== "file") {
        return;
      }
      if (!vscode.languages.match({ pattern: TEST_FILE_GLOB, scheme: "file" }, doc)) {
        return;
      }
      pendingUris.add(doc.uri.toString());
      if (debounceTimer) {
        clearTimeout(debounceTimer);
      }
      debounceTimer = setTimeout(() => {
        debounceTimer = undefined;
        const uris = [...pendingUris];
        pendingUris.clear();
        void queryFiles(uris);
      }, DISCOVERY_DEBOUNCE_MS);
    }),
  );
  context.subscriptions.push({
    dispose: () => {
      if (debounceTimer) {
        clearTimeout(debounceTimer);
      }
    },
  });

  const makeHandler =
    (kind: vscode.TestRunProfileKind) =>
    async (request: vscode.TestRunRequest, token: vscode.CancellationToken) => {
      await runTests(controller, kind, request, token);
    };
  const runProfile = controller.createRunProfile(
    "Run",
    vscode.TestRunProfileKind.Run,
    makeHandler(vscode.TestRunProfileKind.Run),
    true,
  );
  const debugProfile = controller.createRunProfile(
    "Debug",
    vscode.TestRunProfileKind.Debug,
    makeHandler(vscode.TestRunProfileKind.Debug),
    true,
  );

  return { controller, runProfile, debugProfile };
}

// ---------------------------------------------------------------------------
// Execution.
// ---------------------------------------------------------------------------

/** Monotonic suffix so concurrent runs' debug sessions never cross-match. */
let runCounter = 0;

async function runTests(
  controller: vscode.TestController,
  kind: vscode.TestRunProfileKind,
  request: vscode.TestRunRequest,
  token: vscode.CancellationToken,
): Promise<void> {
  const targets = collectTargets(controller, request);

  // Trust gate FIRST: tests are project code (same load-bearing refusal as
  // debugging/javac — the server has no notion of Workspace Trust).
  if (!vscode.workspace.isTrusted) {
    void vscode.window.showErrorMessage(UNTRUSTED_MESSAGE);
    const run = controller.createTestRun(request);
    for (const item of targets) {
      run.errored(item, new vscode.TestMessage(UNTRUSTED_MESSAGE));
    }
    run.end();
    return;
  }

  const run = controller.createTestRun(request);
  try {
    if (targets.length === 0) {
      return;
    }
    const launcherJar = await ensureConsoleLauncher();
    if (!launcherJar) {
      const message = "JUnit console launcher unavailable — download declined or failed.";
      for (const item of targets) {
        run.errored(item, new vscode.TestMessage(message));
      }
      return;
    }

    // Group by workspace folder: each folder is one launch (its own
    // classpath derivation and cwd).
    const byFolder = new Map<string, { folder: vscode.WorkspaceFolder; items: vscode.TestItem[] }>();
    for (const item of targets) {
      const folder = item.uri && vscode.workspace.getWorkspaceFolder(item.uri);
      if (!folder) {
        run.errored(item, new vscode.TestMessage("test file is outside every workspace folder"));
        continue;
      }
      const key = folder.uri.toString();
      const group = byFolder.get(key) ?? { folder, items: [] };
      group.items.push(item);
      byFolder.set(key, group);
    }
    for (const { folder, items } of byFolder.values()) {
      if (token.isCancellationRequested) {
        break;
      }
      await runFolderGroup(run, kind, folder, items, launcherJar, token);
    }
  } finally {
    run.end();
  }
}

/** The requested items, with an empty request meaning "every known top-level
 * class" (never `--scan-classpath`, which would execute dependency-jar
 * tests). */
function collectTargets(
  controller: vscode.TestController,
  request: vscode.TestRunRequest,
): vscode.TestItem[] {
  if (request.include && request.include.length > 0) {
    return [...request.include];
  }
  const classes: vscode.TestItem[] = [];
  controller.items.forEach((fileItem) => {
    fileItem.children.forEach((classItem) => {
      const data = itemData.get(classItem);
      if (data?.type === "class" && !data.fqn.includes("$")) {
        classes.push(classItem);
      }
    });
  });
  return classes;
}

async function runFolderGroup(
  run: vscode.TestRun,
  kind: vscode.TestRunProfileKind,
  folder: vscode.WorkspaceFolder,
  items: vscode.TestItem[],
  launcherJar: string,
  token: vscode.CancellationToken,
): Promise<void> {
  const selectors: string[] = [];
  for (const item of items) {
    for (const selector of selectorsFor(item)) {
      if (!selectors.includes(selector)) {
        selectors.push(selector);
      }
    }
  }
  if (selectors.length === 0) {
    return;
  }
  for (const item of items) {
    markStarted(run, item);
  }

  const reportsDir = fs.mkdtempSync(path.join(os.tmpdir(), "jvl-test-"));
  try {
    runCounter += 1;
    const config: vscode.DebugConfiguration = {
      type: "java-vsix-lite",
      request: "launch",
      name: `Run Java tests #${runCounter}`,
      mainClass: "org.junit.platform.console.ConsoleLauncher",
      includeTestOutputs: true,
      additionalClassPaths: [launcherJar],
      args: ["execute", ...selectors, `--reports-dir=${reportsDir}`, "--disable-banner"],
      projectRoot: folder.uri.fsPath,
      cwd: folder.uri.fsPath,
    };
    const ended = waitForSessionEnd(config.name, folder, token);
    const started = await vscode.debug.startDebugging(
      folder,
      config,
      kind === vscode.TestRunProfileKind.Debug ? {} : { noDebug: true },
    );
    if (!started) {
      ended.cancel();
      for (const item of items) {
        run.errored(item, new vscode.TestMessage("could not start the test launch"));
      }
      return;
    }
    await ended.promise;

    const cases = parseJUnitXml(reportsDir);
    if (cases.length === 0) {
      const message = "test run produced no report (launch failed?)";
      for (const item of items) {
        run.errored(item, new vscode.TestMessage(message));
      }
      return;
    }
    reportCases(run, items, cases);
  } finally {
    fs.rmSync(reportsDir, { recursive: true, force: true });
  }
}

function selectorsFor(item: vscode.TestItem): string[] {
  const data = itemData.get(item);
  if (!data) {
    return [];
  }
  switch (data.type) {
    case "method":
      return [`--select-method=${data.fqn}#${data.method}`];
    case "class":
      return [`--select-class=${data.fqn}`];
    case "file": {
      const out: string[] = [];
      item.children.forEach((child) => {
        out.push(...selectorsFor(child));
      });
      return out;
    }
  }
}

function markStarted(run: vscode.TestRun, item: vscode.TestItem): void {
  const data = itemData.get(item);
  if (data?.type === "method") {
    run.started(item);
    return;
  }
  item.children.forEach((child) => markStarted(run, child));
}

/** Resolve when the named debug session (in `folder`) terminates. Listeners
 * are registered before `startDebugging` so a fast session can't slip by. */
function waitForSessionEnd(
  name: string,
  folder: vscode.WorkspaceFolder,
  token: vscode.CancellationToken,
): { promise: Promise<void>; cancel: () => void } {
  const disposables: vscode.Disposable[] = [];
  let finish: () => void;
  const promise = new Promise<void>((resolve) => {
    finish = () => {
      for (const disposable of disposables) {
        disposable.dispose();
      }
      resolve();
    };
    let session: vscode.DebugSession | undefined;
    const matches = (candidate: vscode.DebugSession): boolean =>
      candidate.name === name &&
      candidate.workspaceFolder?.uri.toString() === folder.uri.toString();
    disposables.push(
      vscode.debug.onDidStartDebugSession((candidate) => {
        if (matches(candidate)) {
          session = candidate;
        }
      }),
    );
    disposables.push(
      vscode.debug.onDidTerminateDebugSession((candidate) => {
        if (matches(candidate)) {
          finish();
        }
      }),
    );
    disposables.push(
      token.onCancellationRequested(() => {
        if (session) {
          void vscode.debug.stopDebugging(session);
        } else {
          finish();
        }
      }),
    );
  });
  return { promise, cancel: () => finish() };
}

// ---------------------------------------------------------------------------
// Launcher jar provisioning.
// ---------------------------------------------------------------------------

/**
 * The console-standalone jar's `~/.m2` path, downloading it (HTTPS +
 * checksum, one modal consent) when absent. `undefined` on decline/failure —
 * the caller reports and aborts, never a partial run.
 */
async function ensureConsoleLauncher(): Promise<string | undefined> {
  const configured = vscode.workspace
    .getConfiguration("java-vsix-lite")
    .get<string>("test.junitLauncherVersion", DEFAULT_LAUNCHER_VERSION)
    .trim();
  const coord: mavenFetch.Coordinate = {
    group: "org.junit.platform",
    artifact: "junit-platform-console-standalone",
    version: configured.length > 0 ? configured : DEFAULT_LAUNCHER_VERSION,
  };
  if (!mavenFetch.isSafeCoordinate(coord)) {
    void vscode.window.showErrorMessage(
      `java-vsix-lite: the configured junitLauncherVersion "${coord.version}" contains unsafe characters — refused.`,
    );
    return undefined;
  }
  const m2Root = path.join(os.homedir(), ".m2", "repository");
  const jarPath = mavenFetch.m2FilePath(m2Root, coord, "jar");
  if (fs.existsSync(jarPath)) {
    return jarPath;
  }
  const choice = await vscode.window.showWarningMessage(
    `Running tests needs the JUnit console launcher (${coord.group}:${coord.artifact}:${coord.version}). ` +
      "Download it over HTTPS (checksum-verified) and install it into ~/.m2/repository? " +
      "Nothing from the workspace decides this URL; the version is the java-vsix-lite.test.junitLauncherVersion setting.",
    { modal: true },
    "Download",
  );
  if (choice !== "Download") {
    return undefined;
  }
  const outcome = await mavenFetch.fetchAndInstallArtifact(coord, m2Root, 64 * 1024 * 1024);
  if (outcome.status === "failed") {
    void vscode.window.showErrorMessage(
      `java-vsix-lite: downloading the JUnit console launcher failed: ${outcome.reason}`,
    );
    return undefined;
  }
  return jarPath;
}

// ---------------------------------------------------------------------------
// JUnit XML report parsing.
//
// The launcher's `--reports-dir` output is the legacy, flat JUnit XML schema
// (one `TEST-<engine>.xml` per engine). The subset we need — `<testcase>`
// attributes plus `<failure>`/`<error>`/`<skipped>` children — is stable and
// regular enough for a small dedicated parser; a new npm dependency for this
// would grow the audited surface for no gain.
// ---------------------------------------------------------------------------

interface ReportCase {
  classname: string;
  name: string;
  durationMs: number;
  /** Failure/error text; `undefined` means passed (or skipped). */
  failure?: string;
  skipped: boolean;
}

function unescapeXml(text: string): string {
  return text
    .replace(/&lt;/g, "<")
    .replace(/&gt;/g, ">")
    .replace(/&quot;/g, '"')
    .replace(/&apos;/g, "'")
    .replace(/&amp;/g, "&");
}

/** Element body → readable text: CDATA sections verbatim, the rest with tags
 * dropped and entities unescaped. */
function bodyText(body: string): string {
  const cdata: string[] = [];
  const withoutCdata = body.replace(/<!\[CDATA\[([\s\S]*?)\]\]>/g, (_, inner: string) => {
    cdata.push(inner);
    return "";
  });
  const plain = unescapeXml(withoutCdata.replace(/<[^>]*>/g, "")).trim();
  return [plain, ...cdata.map((c) => c.trim())].filter((part) => part.length > 0).join("\n");
}

function attributes(tag: string): Map<string, string> {
  const out = new Map<string, string>();
  const re = /([\w.-]+)\s*=\s*"([^"]*)"/g;
  let match: RegExpExecArray | null;
  while ((match = re.exec(tag)) !== null) {
    out.set(match[1], unescapeXml(match[2]));
  }
  return out;
}

/** Parse one report file's `<testcase>` elements. Exported for tests. */
export function parseJUnitXmlText(xml: string): ReportCase[] {
  const cases: ReportCase[] = [];
  const caseRe = /<testcase\b([^>]*?)(\/>|>([\s\S]*?)<\/testcase>)/g;
  let match: RegExpExecArray | null;
  while ((match = caseRe.exec(xml)) !== null) {
    const attrs = attributes(match[1]);
    const name = attrs.get("name");
    const classname = attrs.get("classname");
    if (!name || !classname) {
      continue;
    }
    const seconds = Number(attrs.get("time") ?? "0");
    const body = match[3] ?? "";
    const failMatch = /<(failure|error)\b([^>]*?)(\/>|>([\s\S]*?)<\/\1>)/.exec(body);
    let failure: string | undefined;
    if (failMatch) {
      const message = attributes(failMatch[2]).get("message") ?? "";
      const detail = bodyText(failMatch[4] ?? "");
      failure = [message, detail].filter((part) => part.length > 0).join("\n") || "test failed";
    }
    cases.push({
      classname,
      name,
      durationMs: Number.isFinite(seconds) ? Math.round(seconds * 1000) : 0,
      failure,
      skipped: /<skipped\b/.test(body),
    });
  }
  return cases;
}

/** Every `TEST-*.xml` in `reportsDir`, parsed and concatenated. */
function parseJUnitXml(reportsDir: string): ReportCase[] {
  let entries: string[];
  try {
    entries = fs.readdirSync(reportsDir);
  } catch {
    return [];
  }
  const cases: ReportCase[] = [];
  for (const entry of entries) {
    if (!/^TEST-.*\.xml$/.test(entry)) {
      continue;
    }
    try {
      cases.push(...parseJUnitXmlText(fs.readFileSync(path.join(reportsDir, entry), "utf8")));
    } catch {
      // An unreadable report file degrades to "no result" for its cases.
    }
  }
  return cases;
}

// ---------------------------------------------------------------------------
// Result mapping.
// ---------------------------------------------------------------------------

/** `eachCase(int)[1]` / `adds()` → `eachCase` / `adds` — parameterized and
 * templated invocations aggregate onto their declared method item. */
function methodKey(name: string): string {
  return name.replace(/[([].*$/, "");
}

function reportCases(run: vscode.TestRun, items: vscode.TestItem[], cases: ReportCase[]): void {
  const methodsByKey = new Map<string, vscode.TestItem>();
  const classesByFqn = new Map<string, vscode.TestItem>();
  const index = (item: vscode.TestItem): void => {
    const data = itemData.get(item);
    if (data?.type === "method") {
      methodsByKey.set(`${data.fqn}#${data.method}`, item);
    } else if (data?.type === "class") {
      classesByFqn.set(data.fqn, item);
    }
    item.children.forEach(index);
  };
  for (const item of items) {
    index(item);
  }

  for (const reportCase of cases) {
    const item =
      methodsByKey.get(`${reportCase.classname}#${reportCase.name}`) ??
      methodsByKey.get(`${reportCase.classname}#${methodKey(reportCase.name)}`) ??
      classesByFqn.get(reportCase.classname);
    if (!item) {
      continue; // a testcase outside the requested selection
    }
    if (reportCase.skipped) {
      run.skipped(item);
    } else if (reportCase.failure !== undefined) {
      run.failed(item, new vscode.TestMessage(reportCase.failure), reportCase.durationMs);
    } else {
      run.passed(item, reportCase.durationMs);
    }
  }
}
