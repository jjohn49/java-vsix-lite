#!/usr/bin/env node
// Local equivalent of release.yml's per-platform build: compiles jvl-server
// for the *current* host, copies it into editors/vscode/server/, and only
// then runs `vsce package`.
//
// Why "fail loudly": server/ is gitignored and generated at build/release
// time (see release.yml's "Bundle the platform server binary" step). If we
// let `vsce package` run without the binary in place, it happily produces a
// VSIX that installs and activates but can never start a language server —
// a silent, hard-to-diagnose regression. So this script refuses to package
// unless it can first prove the binary is on disk and executable.
//
// Node built-ins only — no new npm dependency.
import { spawnSync } from "node:child_process";
import { constants as fsConstants, existsSync, mkdirSync, copyFileSync, chmodSync, statSync, accessSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import process from "node:process";

const scriptDir = dirname(fileURLToPath(import.meta.url));
const vscodeDir = resolve(scriptDir, ".."); // editors/vscode
const repoRoot = resolve(vscodeDir, "..", ".."); // repo root

const args = process.argv.slice(2);
const noBuild = args.includes("--no-build");

/** Run a command, streaming its output, and exit the script if it fails. */
function run(command, cmdArgs, options = {}) {
  console.log(`> ${command} ${cmdArgs.join(" ")}`);
  const result = spawnSync(command, cmdArgs, { stdio: "inherit", ...options });
  if (result.error) {
    console.error(`error: failed to run \`${command}\`: ${result.error.message}`);
    process.exit(1);
  }
  if (result.status !== 0) {
    console.error(`error: \`${command} ${cmdArgs.join(" ")}\` exited with code ${result.status}`);
    process.exit(result.status ?? 1);
  }
}

// Mirrors extension.ts's resolveServerPath(): the bundled binary is looked
// up at context.asAbsolutePath(path.join("server", binary)), i.e.
// editors/vscode/server/jvl-server(.exe).
const isWindows = process.platform === "win32";
const binaryName = isWindows ? "jvl-server.exe" : "jvl-server";

// Best-effort mapping to the same --target flavors release.yml packages,
// so a locally-built VSIX matches what CI would ship for this host. An
// unmapped host (e.g. a Linux distro variant) just packages without
// --target — still correct, just not platform-restricted.
function vscePlatformTarget() {
  const { platform, arch } = process;
  const table = {
    "darwin:x64": "darwin-x64",
    "darwin:arm64": "darwin-arm64",
    "linux:x64": "linux-x64",
    "linux:arm64": "linux-arm64",
    "win32:x64": "win32-x64",
  };
  return table[`${platform}:${arch}`];
}

if (noBuild) {
  console.log("--no-build passed: skipping `cargo build`, using whatever is already in target/release/.");
} else {
  run("cargo", ["build", "--release", "-p", "jvl-server"], { cwd: repoRoot });
}

const builtServerPath = join(repoRoot, "target", "release", binaryName);
if (!existsSync(builtServerPath)) {
  console.error(
    `error: expected the built server at ${builtServerPath} but it does not exist.\n` +
      (noBuild
        ? "       (--no-build was passed — run without it, or `cargo build --release -p jvl-server` first.)"
        : "       cargo build reported success but the binary is missing — check the build output above."),
  );
  process.exit(1);
}

const serverDir = join(vscodeDir, "server");
const destServerPath = join(serverDir, binaryName);

mkdirSync(serverDir, { recursive: true });
copyFileSync(builtServerPath, destServerPath);
if (!isWindows) {
  chmodSync(destServerPath, 0o755);
}

// Verify the destination is really there and executable *before* packaging —
// this is the load-bearing check: a missing/non-executable binary here must
// stop the script, never fall through to a silently serverless VSIX.
try {
  const stat = statSync(destServerPath);
  if (!stat.isFile()) {
    throw new Error("not a regular file");
  }
  accessSync(destServerPath, fsConstants.X_OK);
} catch (err) {
  console.error(
    `error: bundled server binary is missing or not executable at ${destServerPath}\n` +
      `       (${err.message})\n` +
      "       Refusing to package a VSIX without a working jvl-server.",
  );
  process.exit(1);
}

console.log(`bundled server verified at ${destServerPath}`);

// Package. Prefer the locally installed vsce (from devDependencies) so this
// works offline once `npm ci` has run; npx will fall back to node_modules/.bin
// automatically, and --no-install stops it from ever reaching the network.
const vsceArgs = ["--no-install", "@vscode/vsce", "package", "--no-dependencies"];
const target = vscePlatformTarget();
if (target) {
  vsceArgs.push("--target", target);
} else {
  console.warn(`warning: no known vsce --target for ${process.platform}/${process.arch}; packaging without --target.`);
}

run("npx", vsceArgs, { cwd: vscodeDir });

console.log("done: VSIX packaged with the native jvl-server bundled.");
