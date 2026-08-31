// Trusted-workspace Test Explorer E2E for the JUnit support in `testing.ts`:
//
// 1. Discovery — the `jvl/tests` request populates the controller with the
//    `junit-mod` fixture's class and method items (static, no JDK needed).
// 2. Execution — the Run profile launches the JUnit Platform Console
//    Launcher through the DAP adapter and reports 2 passed / 1 failed from
//    the `--reports-dir` XML. Requires a JDK (same skip pattern as
//    `debug.test.ts`) and the console-standalone jar in `~/.m2` — absent
//    that jar the leg skips, unless JVL_TEST_REQUIRE_JAVAC=1 (CI) in which
//    case it is downloaded in `suiteSetup` so the run is always exercised.
import * as assert from "assert";
import { spawnSync } from "child_process";
import * as fs from "fs";
import * as os from "os";
import * as path from "path";
import * as vscode from "vscode";

import * as mavenFetch from "../../mavenFetch";
import type { ExtensionApi } from "../../extension";

const FIXTURE = path.resolve(__dirname, "../../../test-fixture");
const JUNIT_MOD = path.join(FIXTURE, "junit-mod");
const LAUNCHER_VERSION = "1.13.4";

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

async function extensionApi(): Promise<ExtensionApi> {
  const ext = vscode.extensions.getExtension("java-vsix-lite.java-vsix-lite");
  assert.ok(ext, "extension not found under id java-vsix-lite.java-vsix-lite");
  const api = (await ext!.activate()) as ExtensionApi;
  assert.ok(api?.testing, "activate() must export the testing API");
  return api;
}

/** The `demo.CalcTest` class item, refreshing discovery until it appears
 *  (the language client may still be starting when the suite begins). */
async function discoverCalcTest(api: ExtensionApi, timeoutMs: number): Promise<vscode.TestItem> {
  const controller = api.testing.controller;
  const find = (): vscode.TestItem | undefined => {
    let found: vscode.TestItem | undefined;
    controller.items.forEach((fileItem) => {
      const candidate = fileItem.children.get(`${fileItem.id}#demo.CalcTest`);
      if (candidate) {
        found = candidate;
      }
    });
    return found;
  };
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline && !find()) {
    await controller.refreshHandler?.(new vscode.CancellationTokenSource().token);
    if (!find()) {
      await new Promise((r) => setTimeout(r, 500));
    }
  }
  const item = find();
  assert.ok(item, "discovery never produced a TestItem for demo.CalcTest");
  return item!;
}

suite("JUnit test explorer (trusted workspace)", () => {
  test("discovers CalcTest with its three methods and ranges", async function () {
    this.timeout(90_000);
    const api = await extensionApi();
    const classItem = await discoverCalcTest(api, 60_000);

    assert.strictEqual(classItem.label, "CalcTest");
    assert.strictEqual(classItem.range?.start.line, 5, "class name range line");
    assert.strictEqual(classItem.children.size, 3, "expected 3 method items");

    const methods = new Map<string, vscode.TestItem>();
    classItem.children.forEach((m) => methods.set(m.label, m));
    assert.deepStrictEqual(
      [...methods.keys()].sort(),
      ["addsSmallNumbers", "addsWrongExpectation", "legacyAdds"],
    );
    assert.strictEqual(methods.get("addsSmallNumbers")!.range?.start.line, 7);
    assert.strictEqual(methods.get("addsWrongExpectation")!.range?.start.line, 12);
    assert.strictEqual(methods.get("legacyAdds")!.range?.start.line, 17);
  });

  suite("execution", () => {
    const requireRun = process.env.JVL_TEST_REQUIRE_JAVAC === "1";
    const m2Root = path.join(os.homedir(), ".m2", "repository");
    const launcherCoord: mavenFetch.Coordinate = {
      group: "org.junit.platform",
      artifact: "junit-platform-console-standalone",
      version: LAUNCHER_VERSION,
    };
    const launcherJar = mavenFetch.m2FilePath(m2Root, launcherCoord, "jar");
    let ready = false;

    suiteSetup(async function () {
      this.timeout(120_000);
      const javac = locateJavac();
      if (!javac) {
        this.skip();
        return;
      }
      if (!fs.existsSync(launcherJar)) {
        if (!requireRun) {
          this.skip();
          return;
        }
        // CI: seed the launcher (HTTPS + checksum, same code path as the
        // extension's consent-gated download).
        const outcome = await mavenFetch.fetchAndInstallArtifact(
          launcherCoord,
          m2Root,
          64 * 1024 * 1024,
        );
        assert.strictEqual(
          outcome.status,
          "downloaded",
          `could not seed the console launcher: ${JSON.stringify(outcome)}`,
        );
      }
      // Pre-build the fixture module so the derived-classpath mode is used
      // (the auto-compile fallback would try to compile the whole fixture,
      // including the deliberately-broken Broken.java).
      const classes = path.join(JUNIT_MOD, "target", "classes");
      const testClasses = path.join(JUNIT_MOD, "target", "test-classes");
      fs.mkdirSync(classes, { recursive: true });
      fs.mkdirSync(testClasses, { recursive: true });
      const mainResult = spawnSync(
        javac,
        ["-g", "-d", classes, path.join(JUNIT_MOD, "src", "main", "java", "demo", "Calc.java")],
        { encoding: "utf8" },
      );
      assert.strictEqual(mainResult.status, 0, `javac (main) failed: ${mainResult.stderr}`);
      const testResult = spawnSync(
        javac,
        [
          "-g",
          "-cp",
          [classes, launcherJar].join(path.delimiter),
          "-d",
          testClasses,
          path.join(JUNIT_MOD, "src", "test", "java", "demo", "CalcTest.java"),
        ],
        { encoding: "utf8" },
      );
      assert.strictEqual(testResult.status, 0, `javac (test) failed: ${testResult.stderr}`);
      ready = true;
    });

    suiteTeardown(() => {
      fs.rmSync(path.join(JUNIT_MOD, "target"), { recursive: true, force: true });
    });

    test("Run profile reports 2 passed and 1 failed with the assertion text", async function () {
      if (!ready) {
        this.skip();
        return;
      }
      this.timeout(120_000);
      const api = await extensionApi();
      const classItem = await discoverCalcTest(api, 60_000);
      const controller = api.testing.controller;

      // Spy on the controller's runs to observe reported results — the Test
      // Explorer has no public read API for outcomes (same interception
      // pattern as the untrusted suite's captureErrorMessage).
      const passed: string[] = [];
      const failed: { id: string; message: string }[] = [];
      const target = controller as { createTestRun: vscode.TestController["createTestRun"] };
      const originalCreate = target.createTestRun.bind(controller);
      target.createTestRun = (request, name?, persist?) => {
        const run = originalCreate(request, name, persist);
        const originalPassed = run.passed.bind(run);
        const originalFailed = run.failed.bind(run);
        run.passed = (item, duration) => {
          passed.push(item.id);
          originalPassed(item, duration);
        };
        run.failed = (item, message, duration) => {
          const text = Array.isArray(message)
            ? message.map((m) => String(m.message)).join("\n")
            : String((message as vscode.TestMessage).message);
          failed.push({ id: item.id, message: text });
          originalFailed(item, message, duration);
        };
        return run;
      };
      try {
        const tokenSource = new vscode.CancellationTokenSource();
        await api.testing.runProfile.runHandler!(
          new vscode.TestRunRequest([classItem]),
          tokenSource.token,
        );
      } finally {
        target.createTestRun = originalCreate;
      }

      assert.strictEqual(
        passed.length,
        2,
        `expected exactly 2 passed tests, got ${JSON.stringify({ passed, failed })}`,
      );
      assert.strictEqual(
        failed.length,
        1,
        `expected exactly 1 failed test, got ${JSON.stringify({ passed, failed })}`,
      );
      assert.ok(
        failed[0].id.endsWith("#demo.CalcTest#addsWrongExpectation"),
        `wrong failing test: ${failed[0].id}`,
      );
      assert.ok(
        /expected/i.test(failed[0].message) && failed[0].message.includes("3"),
        `failure message must carry the assertion text, got: ${failed[0].message}`,
      );
    });
  });
});
