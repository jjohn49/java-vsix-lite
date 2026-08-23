// Integration-test launcher: downloads a VS Code build, loads this extension
// from source (dist bundle), opens the fixture workspace, and runs the mocha
// suite inside the extension host. The Rust server binary must already be
// built (`cargo build -p jvl-server`); its path is handed to the extension
// via JVL_SERVER_PATH, same as the F5 launch config.
//
// Two SEPARATE `runTests()` invocations (two separate extension-host
// launches, each with its own throwaway user-data-dir): the trusted suite
// below, unchanged from before, and the untrusted-workspace suite added
// alongside it (see `runUntrustedSuite`) -- security-relevant trust
// behavior needs the opposite Workspace Trust setup from the trusted suite,
// so it gets its own launch rather than weakening this one.
import * as fs from "fs";
import * as os from "os";
import * as path from "path";
import { runTests } from "@vscode/test-electron";

async function runTrustedSuite(
  extensionDevelopmentPath: string,
  serverBinary: string,
): Promise<void> {
  const fixtureWorkspace = path.resolve(extensionDevelopmentPath, "test-fixture");

  await runTests({
    extensionDevelopmentPath,
    extensionTestsPath: path.resolve(__dirname, "./suite/index"),
    launchArgs: [
      fixtureWorkspace,
      // The suite asserts on diagnostics from THIS extension only — a
      // clean profile keeps other Java extensions out of the picture.
      "--disable-extensions",
      // The background javac check is trust-gated; the harness has no UI
      // driver to accept the trust prompt, so trust checks are disabled
      // (equivalent to an explicitly trusted workspace).
      "--disable-workspace-trust",
      // A short user-data dir: the default lives under the (possibly deep)
      // repo path, and VS Code's IPC socket path has a ~103-char unix limit.
      `--user-data-dir=${fs.mkdtempSync(path.join(os.tmpdir(), "jvl-ud-"))}`,
    ],
    extensionTestsEnv: {
      JVL_SERVER_PATH: serverBinary,
    },
  });
}

async function runUntrustedSuite(
  extensionDevelopmentPath: string,
  serverBinary: string,
): Promise<void> {
  const fixtureWorkspace = path.resolve(extensionDevelopmentPath, "test-fixture-untrusted");
  const userDataDir = fs.mkdtempSync(path.join(os.tmpdir(), "jvl-ud-untrusted-"));

  // The opposite setup from `runTrustedSuite`: Workspace Trust stays ENABLED
  // (no `--disable-workspace-trust`) and this fixture folder has never been
  // opened in this fresh profile. `startupPrompt: "never"` only suppresses
  // the interactive "Do you trust the authors" modal (nothing in this
  // headless harness could click through it) -- it does not grant trust.
  //
  // That said, this alone is NOT sufficient: `@vscode/test-electron` always
  // launches via `--extensionDevelopmentPath` (the Extension Development
  // Host), and VS Code unconditionally trusts that host regardless of these
  // settings -- confirmed empirically while writing this suite. See the
  // header comment in `./suiteUntrusted/untrustedWorkspace.test.ts` for how
  // the suite forces the untrusted signal from inside the extension host
  // once it's running. This launch setup is still worth keeping: it's the
  // correct real-world configuration, and if a future VS Code version
  // changes the Extension Development Host's trust behavior, this is what
  // would make the workspace genuinely untrusted with no further change.
  fs.mkdirSync(path.join(userDataDir, "User"), { recursive: true });
  fs.writeFileSync(
    path.join(userDataDir, "User", "settings.json"),
    JSON.stringify(
      {
        "security.workspace.trust.enabled": true,
        "security.workspace.trust.startupPrompt": "never",
        "security.workspace.trust.banner": "never",
      },
      null,
      2,
    ),
  );

  await runTests({
    extensionDevelopmentPath,
    extensionTestsPath: path.resolve(__dirname, "./suiteUntrusted/index"),
    launchArgs: [
      fixtureWorkspace,
      "--disable-extensions",
      `--user-data-dir=${userDataDir}`,
    ],
    extensionTestsEnv: {
      JVL_SERVER_PATH: serverBinary,
    },
  });
}

async function main(): Promise<void> {
  const extensionDevelopmentPath = path.resolve(__dirname, "../../");
  const serverBinary =
    process.env.JVL_SERVER_PATH ??
    path.resolve(
      extensionDevelopmentPath,
      "../../target/debug",
      process.platform === "win32" ? "jvl-server.exe" : "jvl-server",
    );

  await runTrustedSuite(extensionDevelopmentPath, serverBinary);
  await runUntrustedSuite(extensionDevelopmentPath, serverBinary);
}

main().catch((err) => {
  console.error("integration tests failed:", err);
  process.exit(1);
});
