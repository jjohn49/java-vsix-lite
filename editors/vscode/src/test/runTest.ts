// Integration-test launcher: downloads a VS Code build, loads this extension
// from source (dist bundle), opens the fixture workspace, and runs the mocha
// suite inside the extension host. The Rust server binary must already be
// built (`cargo build -p jvl-server`); its path is handed to the extension
// via JVL_SERVER_PATH, same as the F5 launch config.
import * as fs from "fs";
import * as os from "os";
import * as path from "path";
import { runTests } from "@vscode/test-electron";

async function main(): Promise<void> {
  const extensionDevelopmentPath = path.resolve(__dirname, "../../");
  const extensionTestsPath = path.resolve(__dirname, "./suite/index");
  const fixtureWorkspace = path.resolve(extensionDevelopmentPath, "test-fixture");
  const serverBinary =
    process.env.JVL_SERVER_PATH ??
    path.resolve(
      extensionDevelopmentPath,
      "../../target/debug",
      process.platform === "win32" ? "jvl-server.exe" : "jvl-server",
    );

  await runTests({
    extensionDevelopmentPath,
    extensionTestsPath,
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

main().catch((err) => {
  console.error("integration tests failed:", err);
  process.exit(1);
});
