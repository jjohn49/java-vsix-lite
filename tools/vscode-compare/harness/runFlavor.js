// Runs the probe suite for exactly ONE flavor, selected by
// $JVL_COMPARE_FLAVOR. run.sh launches this image once per flavor so each
// gets its own pristine container: no warm page cache, JIT state, disk
// churn, or memory pressure carried over from the other one, and the cgroup
// stats sampled inside each container describe that flavor alone.
const cp = require("node:child_process");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");

const { runTests, resolveCliArgsFromVSCodeExecutablePath } = require("@vscode/test-electron");

const HARNESS_DIR = __dirname;
const ROOT = path.resolve(HARNESS_DIR, "..");
const PROJECT = process.env.JVL_COMPARE_PROJECT ?? path.join(ROOT, "project");
const OUT_DIR = process.env.JVL_COMPARE_OUT_DIR ?? "/out";
const VSCODE_EXEC = fs.readFileSync("/opt/vscode-exec-path.txt", "utf8").trim();
const VSCODE_CACHE = "/opt/vscode-test-cache";

// Quiet period between finishing setup (container boot, Xvfb, extension
// install) and starting to sample, so one-time startup cost stays out of
// the measured window. Override with JVL_COMPARE_SETTLE_SECONDS=0 for fast
// iteration when you only care about the probe values.
const SETTLE_MS = Math.max(0, Number(process.env.JVL_COMPARE_SETTLE_SECONDS ?? 30)) * 1000;

// `none` installs nothing: it measures what the container, VS Code, and the
// probe suite itself cost with no Java support present at all, which is the
// floor both plugins should be read against.
const VSIX_BY_FLAVOR = {
  none: null,
  ours: "/opt/vsix/java-vsix-lite.vsix",
  redhat: "/opt/vsix/redhat-java.vsix",
};

/** Install one VSIX into a private extensions dir using VS Code's own CLI. */
function installVsix(extensionsDir, userDataDir, vsix) {
  const [cli, ...args] = resolveCliArgsFromVSCodeExecutablePath(VSCODE_EXEC);
  const result = cp.spawnSync(
    cli,
    [
      ...args,
      `--extensions-dir=${extensionsDir}`,
      `--user-data-dir=${userDataDir}`,
      "--install-extension",
      vsix,
      "--force",
    ],
    { encoding: "utf-8", stdio: "inherit" },
  );
  if (result.status !== 0) {
    throw new Error(`installing ${vsix} failed with status ${result.status}`);
  }
}

// VS Code-level settings, identical for both flavors: no trust prompts, no
// telemetry, no update/experiment traffic.
const SHARED_SETTINGS = {
  "security.workspace.trust.enabled": false,
  "telemetry.telemetryLevel": "off",
  "update.mode": "none",
  "extensions.autoUpdate": false,
  "extensions.autoCheckUpdates": false,
  "workbench.enableExperiments": false,
};

// Per-flavor settings, written only into that flavor's container so neither
// plugin ever sees the other's configuration keys. Both sets say the same
// thing in each plugin's own vocabulary: import the Maven project, resolve
// dependencies read-only from the warm ~/.m2, download nothing.
//
// `java.import.maven.enabled` must stay true. With it off, jdt.ls falls
// back to "Orders.java is a non-project file, only syntax errors are
// reported" — it then answers every semantic probe with silence, which
// looks like a fast, clean result and is in fact no analysis at all.
const SETTINGS_BY_FLAVOR = {
  none: {},
  ours: {
    "java-vsix-lite.javac.checkOnSave": false,
    "java-vsix-lite.dependencies.autoDownload": "never",
  },
  redhat: {
    "java.jdt.ls.java.home": process.env.JAVA_HOME,
    "java.import.maven.enabled": true,
    "java.import.gradle.enabled": false,
    "java.configuration.checkProjectSettingsExclusions": false,
    "java.configuration.updateBuildConfiguration": "automatic",
    "java.maven.downloadSources": false,
    "java.eclipse.downloadSources": false,
  },
};

function writeSettings(userDataDir, flavorName) {
  const userDir = path.join(userDataDir, "User");
  fs.mkdirSync(userDir, { recursive: true });
  fs.writeFileSync(
    path.join(userDir, "settings.json"),
    JSON.stringify({ ...SHARED_SETTINGS, ...SETTINGS_BY_FLAVOR[flavorName] }, null, 2),
  );
}

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

async function runFlavor(flavor) {
  const extensionsDir = fs.mkdtempSync(path.join(os.tmpdir(), `jvl-ext-${flavor.name}-`));
  const userDataDir = fs.mkdtempSync(path.join(os.tmpdir(), `jvl-ud-${flavor.name}-`));
  writeSettings(userDataDir, flavor.name);
  if (flavor.vsix) {
    installVsix(extensionsDir, userDataDir, flavor.vsix);
  } else {
    console.log("baseline flavor: installing no extension");
  }

  // Settle before measuring. Container creation, image-layer first-touch
  // page faults, Xvfb boot, and the heavyweight `code --install-extension`
  // Electron run all happen above this line; without a quiet period after
  // them, that one-time setup cost bleeds into the first seconds of the
  // sampled window and is easily mistaken for plugin behavior.
  if (SETTLE_MS > 0) {
    console.log(`settling for ${SETTLE_MS / 1000}s before sampling…`);
    await sleep(SETTLE_MS);
  }

  // Sampling covers exactly the measured window: it starts here and is
  // killed as soon as the probe suite finishes.
  const statsFile = path.join(OUT_DIR, `stats-${flavor.name}.csv`);
  const sampler = cp.spawn(process.execPath, [path.join(HARNESS_DIR, "sampleStats.js"), statsFile], {
    stdio: "ignore",
  });

  try {
    await runTests({
      vscodeExecutablePath: VSCODE_EXEC,
      cachePath: VSCODE_CACHE,
      extensionDevelopmentPath: path.join(HARNESS_DIR, "driver"),
      extensionTestsPath: path.join(HARNESS_DIR, "suite"),
      launchArgs: [
        PROJECT,
        "--disable-workspace-trust",
        "--disable-telemetry",
        "--skip-welcome",
        "--skip-release-notes",
        `--extensions-dir=${extensionsDir}`,
        `--user-data-dir=${userDataDir}`,
      ],
      extensionTestsEnv: {
        JVL_COMPARE_FLAVOR: flavor.name,
        JVL_COMPARE_OUT: path.join(OUT_DIR, `report-${flavor.name}.json`),
        JVL_COMPARE_PROJECT: PROJECT,
        JVL_COMPARE_SETTLE_SECONDS: String(SETTLE_MS / 1000),
      },
    });
  } finally {
    sampler.kill("SIGTERM");
  }
}

async function main() {
  const name = process.env.JVL_COMPARE_FLAVOR;
  if (!name || !Object.hasOwn(VSIX_BY_FLAVOR, name)) {
    throw new Error(
      `set JVL_COMPARE_FLAVOR to one of: ${Object.keys(VSIX_BY_FLAVOR).join(", ")} (got ${name ?? "nothing"})`,
    );
  }
  fs.mkdirSync(OUT_DIR, { recursive: true });
  console.log(`=== running flavor: ${name}`);
  await runFlavor({ name, vsix: VSIX_BY_FLAVOR[name] });
}

main().catch((err) => {
  console.error("flavor run failed:", err);
  process.exit(1);
});
