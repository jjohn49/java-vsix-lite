// M6.2: consent-gated remote dependency fetching — extension-side HTTPS,
// checksum verification, and ~/.m2 install. Deliberately isolated in this one
// small module (Node stdlib only: `https`, `crypto`, `fs`, `path` — no new
// npm dependency) so the network/filesystem surface the threat model cares
// about is easy to review as a whole. Never invoked implicitly: every call
// here is reachable only from `extension.ts`'s `downloadDependencies` command,
// itself gated on Workspace Trust and an explicit, per-invocation consent
// dialog (see that file).
//
// Threat-model notes (see `docs/THREAT_MODEL.md`):
// - TLS only (`https:` scheme enforced below); the platform's default
//   certificate validation applies (Node's `https` module, no custom
//   `rejectUnauthorized`/CA overrides).
// - Checksum verification is mandatory: an artifact with no published
//   `.sha512`/`.sha256`/`.sha1` sidecar is refused, not installed
//   unverified. IMPORTANT: this verifies *integrity* (the bytes match what
//   Maven Central currently serves for that sidecar), not *provenance* — a
//   same-origin checksum published alongside a compromised artifact would
//   still match. It defends against transport corruption and accidental
//   mismatches, not a compromised Maven Central.
// - Nothing downloaded here is ever executed: `.jar` bytes are data consumed
//   only by the existing bytecode parser (`crates/classpath`), `.pom` only by
//   the XXE-safe XML parser. This module doesn't even open either file.
// - Download-then-verify-then-install: every file lands in a temp path in
//   the *same directory* as its final `~/.m2` location first, and is moved
//   into place only after its checksum verifies — so a crash, cancellation,
//   or checksum failure can never leave a partial/unverified file at the
//   real path a rebuild would pick up.

import * as crypto from "crypto";
import * as fs from "fs";
import * as https from "https";
import * as path from "path";

/** A bare Maven coordinate — repo-agnostic, works for both Maven and Gradle projects. */
export interface Coordinate {
  group: string;
  artifact: string;
  version: string;
}

export const MAVEN_CENTRAL_BASE = "https://repo.maven.apache.org/maven2";

/** Checksum sidecars, tried in preference order (strongest first). */
const CHECKSUM_ALGOS: ReadonlyArray<{ ext: string; algo: string }> = [
  { ext: "sha512", algo: "sha512" },
  { ext: "sha256", algo: "sha256" },
  { ext: "sha1", algo: "sha1" },
];

/** Checksum sidecar files are tiny (a hex string); cap generously but firmly. */
const MAX_CHECKSUM_BYTES = 4 * 1024;

/** A single network/timeout attempt's ceiling — separate from the per-invocation budget. */
const DEFAULT_REQUEST_TIMEOUT_MS = 30_000;

/** An HTTP response with a non-2xx status — carries the status for classification. */
export class HttpStatusError extends Error {
  constructor(
    public readonly statusCode: number,
    message: string,
  ) {
    super(message);
    this.name = "HttpStatusError";
  }
}

/** No `.sha512`/`.sha256`/`.sha1` sidecar was published for a file. */
export class NoChecksumError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "NoChecksumError";
  }
}

/** A published checksum didn't match the downloaded bytes. */
export class ChecksumMismatchError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "ChecksumMismatchError";
  }
}

/**
 * One coordinate segment is safe to use in a URL path and a filesystem path
 * if it's non-empty, contains no whitespace, and can't be read as a path
 * traversal or separator — the same conservative shape the Rust side
 * enforces on its own path-building (`crates/classpath::maven::unsafe_coord`).
 * Independently enforced here too: this module builds real filesystem paths
 * from server-supplied strings, so it must never trust the server alone.
 */
export function isSafeCoordinateSegment(segment: string): boolean {
  return (
    segment.length > 0 &&
    !/\s/.test(segment) &&
    !segment.includes("..") &&
    !segment.includes("/") &&
    !segment.includes("\\") &&
    !segment.includes(":")
  );
}

export function isSafeCoordinate(coord: Coordinate): boolean {
  return (
    isSafeCoordinateSegment(coord.group) &&
    isSafeCoordinateSegment(coord.artifact) &&
    isSafeCoordinateSegment(coord.version)
  );
}

function groupPath(group: string): string {
  return group.split(".").join("/");
}

/** `https://repo.maven.apache.org/maven2/<g-path>/<a>/<v>/<a>-<v>.<ext>`. */
export function artifactFileUrl(coord: Coordinate, ext: "pom" | "jar"): string {
  return `${MAVEN_CENTRAL_BASE}/${groupPath(coord.group)}/${coord.artifact}/${coord.version}/${coord.artifact}-${coord.version}.${ext}`;
}

/**
 * The `~/.m2/repository`-relative install path for a coordinate's file, in
 * the exact layout the Rust resolver expects
 * (`crates/classpath::maven::m2_relative_ext`). Throws if the coordinate
 * isn't [`isSafeCoordinate`] — callers must validate first, but this is a
 * fail-closed backstop since it's the function that actually builds a real
 * filesystem path.
 */
export function m2FilePath(m2Root: string, coord: Coordinate, ext: "pom" | "jar"): string {
  if (!isSafeCoordinate(coord)) {
    throw new Error(
      `refusing to build an m2 path for an unsafe coordinate: ${coord.group}:${coord.artifact}:${coord.version}`,
    );
  }
  const segments = [m2Root, ...coord.group.split("."), coord.artifact, coord.version];
  return path.join(...segments, `${coord.artifact}-${coord.version}.${ext}`);
}

/**
 * GET `url` over HTTPS into memory, refusing anything but an `https:` URL and
 * a `200` response, and aborting once the response body would exceed
 * `maxBytes` (checked incrementally as chunks arrive, so an oversized
 * response is never buffered in full before the cap is noticed).
 */
export function httpsGetBuffer(
  url: string,
  maxBytes: number,
  timeoutMs: number = DEFAULT_REQUEST_TIMEOUT_MS,
): Promise<Buffer> {
  return new Promise((resolve, reject) => {
    if (!url.startsWith("https://")) {
      reject(new Error(`refusing a non-HTTPS URL: ${url}`));
      return;
    }
    const req = https.get(url, { timeout: timeoutMs }, (res) => {
      const status = res.statusCode ?? 0;
      if (status !== 200) {
        res.resume(); // drain so the socket can be reused/closed cleanly
        reject(new HttpStatusError(status, `HTTP ${status} for ${url}`));
        return;
      }
      const chunks: Buffer[] = [];
      let total = 0;
      res.on("data", (chunk: Buffer) => {
        total += chunk.length;
        if (total > maxBytes) {
          req.destroy(new Error(`response for ${url} exceeded the ${maxBytes}-byte cap`));
          return;
        }
        chunks.push(chunk);
      });
      res.on("end", () => resolve(Buffer.concat(chunks)));
      res.on("error", reject);
    });
    req.on("timeout", () => req.destroy(new Error(`request to ${url} timed out after ${timeoutMs}ms`)));
    req.on("error", reject);
  });
}

/** The first hex-looking token in a checksum sidecar's text (some publish `hash *filename`, most just `hash`). */
export function extractHex(checksumFileText: string): string | undefined {
  const match = /[0-9a-fA-F]{32,128}/.exec(checksumFileText);
  return match ? match[0].toLowerCase() : undefined;
}

/** One verified download: the bytes plus which algorithm verified them (for logging/diagnostics). */
export interface VerifiedFile {
  data: Buffer;
  algo: string;
}

/**
 * Download one artifact file (`.pom` or `.jar`) and verify it against its
 * published checksum sidecar, preferring `.sha512`, then `.sha256`, then
 * `.sha1` (Maven Central publishes `.sha1` near-universally; the stronger
 * algorithms are newer and not always present). Throws [`NoChecksumError`] if
 * none of the three sidecars can be fetched at all — checksum verification
 * is mandatory, never best-effort (see this module's doc comment).
 */
export async function fetchVerifiedFile(
  coord: Coordinate,
  ext: "pom" | "jar",
  maxBytes: number,
): Promise<VerifiedFile> {
  const fileUrl = artifactFileUrl(coord, ext);
  const data = await httpsGetBuffer(fileUrl, maxBytes);
  for (const { ext: checksumExt, algo } of CHECKSUM_ALGOS) {
    let checksumText: string;
    try {
      const checksumBuf = await httpsGetBuffer(`${fileUrl}.${checksumExt}`, MAX_CHECKSUM_BYTES);
      checksumText = checksumBuf.toString("utf8");
    } catch {
      continue; // this sidecar isn't published — try the next-weaker one
    }
    const expected = extractHex(checksumText);
    if (!expected) {
      continue;
    }
    const actual = crypto.createHash(algo).update(data).digest("hex");
    if (actual !== expected) {
      throw new ChecksumMismatchError(
        `${ext} ${algo} checksum mismatch for ${coord.group}:${coord.artifact}:${coord.version} — discarding the download`,
      );
    }
    return { data, algo };
  }
  throw new NoChecksumError(
    `no .sha512/.sha256/.sha1 checksum published for ${coord.group}:${coord.artifact}:${coord.version}'s ${ext} — refusing to install an unverified download`,
  );
}

/**
 * Write `data` to a temp file in `finalPath`'s own directory, then rename it
 * into place — same filesystem by construction (same directory), so the
 * rename is atomic: `finalPath` either doesn't exist yet or is the complete,
 * verified file, never a partial write.
 */
export async function installVerifiedFile(finalPath: string, data: Buffer): Promise<void> {
  const dir = path.dirname(finalPath);
  await fs.promises.mkdir(dir, { recursive: true });
  const tmpPath = path.join(
    dir,
    `.${path.basename(finalPath)}.tmp-${process.pid}-${crypto.randomBytes(6).toString("hex")}`,
  );
  await fs.promises.writeFile(tmpPath, data);
  try {
    await fs.promises.rename(tmpPath, finalPath);
  } catch (err) {
    await fs.promises.unlink(tmpPath).catch(() => undefined);
    throw err;
  }
}

export type FetchOutcome =
  | { status: "downloaded"; coord: Coordinate; bytes: number }
  | { status: "failed"; coord: Coordinate; reason: string };

/**
 * Human-readable, non-alarming classification of a failure, distinguishing
 * "not found on Maven Central" (per the brief, out-of-scope custom
 * repositories are the likely cause) from checksum/network problems.
 */
function classifyError(err: unknown): string {
  if (err instanceof HttpStatusError) {
    if (err.statusCode === 404) {
      return "not found on Maven Central — may require a custom repository (not supported yet)";
    }
    return `Maven Central returned HTTP ${err.statusCode}`;
  }
  if (err instanceof NoChecksumError || err instanceof ChecksumMismatchError) {
    return err.message;
  }
  if (err instanceof Error) {
    return `network error: ${err.message}`;
  }
  return `network error: ${String(err)}`;
}

/**
 * Fetch, verify, and install one coordinate's `.pom` and `.jar` into
 * `m2Root`. Both-or-neither: the pom and jar are only written to their final
 * `~/.m2` paths after BOTH have downloaded and verified successfully, so a
 * failure partway (e.g. the jar 404s after the pom verified fine) never
 * installs half a coordinate. `remainingBytesBudget` bounds this single
 * artifact's downloads (the caller enforces the whole-invocation 200MB cap
 * by shrinking this on each call).
 */
export async function fetchAndInstallArtifact(
  coord: Coordinate,
  m2Root: string,
  remainingBytesBudget: number,
): Promise<FetchOutcome> {
  if (!isSafeCoordinate(coord)) {
    return {
      status: "failed",
      coord,
      reason: "coordinate contains unsafe characters — refused",
    };
  }
  try {
    const pom = await fetchVerifiedFile(coord, "pom", remainingBytesBudget);
    const jarBudget = remainingBytesBudget - pom.data.length;
    if (jarBudget <= 0) {
      return {
        status: "failed",
        coord,
        reason: "would exceed this invocation's download size cap",
      };
    }
    const jar = await fetchVerifiedFile(coord, "jar", jarBudget);
    // Only now, with both verified, install both — see the doc comment above.
    await installVerifiedFile(m2FilePath(m2Root, coord, "pom"), pom.data);
    await installVerifiedFile(m2FilePath(m2Root, coord, "jar"), jar.data);
    return { status: "downloaded", coord, bytes: pom.data.length + jar.data.length };
  } catch (err) {
    return { status: "failed", coord, reason: classifyError(err) };
  }
}
