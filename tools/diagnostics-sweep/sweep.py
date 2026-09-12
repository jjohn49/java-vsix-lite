#!/usr/bin/env python3
"""Sweep a real Java project through jvl-server and report every diagnostic.

The point is finding *false positives*: run a project that a compiler accepts,
and anything reported is suspect. This found five distinct bugs on cbioportal
(1004 files) where javac reported one genuine error and jvl-server reported
104.

Usage:
    tools/diagnostics-sweep/sweep.py <server-binary> <project-root> [subdir]
        [--out FILE] [--limit N] [--skip N] [--timeout SECONDS]

    # build the server first; a debug binary is ~10x slower but works
    cargo build --release -p jvl-server
    tools/diagnostics-sweep/sweep.py target/release/jvl-server ~/src/cbioportal src/main/java

Getting a trustworthy baseline matters as much as the sweep:

  1. Build the project with its own toolchain first (`mvn -DskipTests compile`).
     That populates `~/.m2`, which is where the classpath layer reads
     dependencies from -- without it most types do not resolve, every check
     goes conservatively silent, and the sweep finds nothing.
  2. Keep javac's error list. Any jvl error not in it is a false-positive
     candidate; that diff is the whole deliverable.
  3. Prefer a module whose build actually succeeds. Errors in a module that
     failed to compile are mostly cascade noise from missing upstream
     classes, not findings.

One document is opened, its diagnostics collected, and it is closed again
before the next. Holding every document open instead makes per-file analysis
degrade badly: 1004 files took over 25 minutes that way and did not finish,
versus 64 seconds like this.

Rows are written as JSON Lines and flushed per file, so a run killed by a
timeout still leaves usable data.
"""

import argparse
import collections
import json
import os
import subprocess
import threading
import time

SEVERITY = {1: "Error", 2: "Warning", 3: "Info", 4: "Hint"}


class Server:
    """Minimal LSP client: just enough to open documents and read diagnostics."""

    def __init__(self, binary, root):
        self.proc = subprocess.Popen(
            [binary],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
        )
        self.diagnostics = {}
        self.lock = threading.Lock()
        threading.Thread(target=self._read_loop, daemon=True).start()
        self._send(
            {
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "processId": os.getpid(),
                    "rootUri": "file://" + root,
                    "workspaceFolders": [{"uri": "file://" + root, "name": "sweep"}],
                    "capabilities": {"general": {"positionEncodings": ["utf-16"]}},
                },
            }
        )
        # The classpath layer resolves the project's dependencies on
        # initialize; probing before that finishes measures a cold server
        # rather than the analysis.
        time.sleep(10)
        self._send({"jsonrpc": "2.0", "method": "initialized", "params": {}})
        time.sleep(5)

    def _send(self, payload):
        body = json.dumps(payload).encode()
        self.proc.stdin.write(b"Content-Length: %d\r\n\r\n%s" % (len(body), body))
        self.proc.stdin.flush()

    def _read_loop(self):
        out = self.proc.stdout
        while True:
            header = b""
            while b"\r\n\r\n" not in header:
                byte = out.read(1)
                if not byte:
                    return
                header += byte
            length = next(
                int(line.split(":")[1])
                for line in header.decode().split("\r\n")
                if line.lower().startswith("content-length")
            )
            body = b""
            while len(body) < length:
                chunk = out.read(length - len(body))
                if not chunk:
                    return
                body += chunk
            try:
                message = json.loads(body)
            except ValueError:
                continue
            if message.get("method") == "textDocument/publishDiagnostics":
                params = message["params"]
                with self.lock:
                    self.diagnostics[params["uri"]] = params["diagnostics"]

    def analyze(self, path, deadline_seconds):
        """Open `path`, wait for its diagnostics, close it. None on timeout."""
        uri = "file://" + path
        try:
            text = open(path, encoding="utf-8").read()
        except (OSError, UnicodeDecodeError):
            return []
        with self.lock:
            self.diagnostics.pop(uri, None)
        self._send(
            {
                "jsonrpc": "2.0",
                "method": "textDocument/didOpen",
                "params": {
                    "textDocument": {
                        "uri": uri,
                        "languageId": "java",
                        "version": 1,
                        "text": text,
                    }
                },
            }
        )
        result = None
        deadline = time.time() + deadline_seconds
        while time.time() < deadline:
            with self.lock:
                # An empty list is a real answer ("nothing wrong here"), so
                # membership is the test, not truthiness.
                if uri in self.diagnostics:
                    result = self.diagnostics[uri]
                    break
            time.sleep(0.05)
        self._send(
            {
                "jsonrpc": "2.0",
                "method": "textDocument/didClose",
                "params": {"textDocument": {"uri": uri}},
            }
        )
        return result

    def close(self):
        self.proc.kill()


def java_files(root, subdir):
    base = os.path.join(root, subdir) if subdir else root
    found = []
    for directory, _, names in os.walk(base):
        found.extend(
            os.path.join(directory, n) for n in names if n.endswith(".java")
        )
    return sorted(found)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("server")
    parser.add_argument("root")
    parser.add_argument("subdir", nargs="?", default="")
    parser.add_argument("--out", default="sweep.jsonl")
    parser.add_argument("--limit", type=int, default=0)
    parser.add_argument("--skip", type=int, default=0)
    parser.add_argument("--timeout", type=float, default=12.0)
    args = parser.parse_args()

    root = os.path.abspath(args.root)
    files = java_files(root, args.subdir)[args.skip :]
    if args.limit:
        files = files[: args.limit]
    print(f"sweeping {len(files)} files under {args.subdir or '.'}", flush=True)

    server = Server(os.path.abspath(args.server), root)
    rows = []
    started = time.time()
    with open(args.out, "w") as out:
        for index, path in enumerate(files):
            relative = path[len(root) + 1 :]
            found = server.analyze(path, args.timeout)
            if found is None:
                print(f"  TIMEOUT {relative}", flush=True)
                out.write(json.dumps({"file": relative, "timeout": True}) + "\n")
                out.flush()
                continue
            for diagnostic in found:
                row = {
                    "file": relative,
                    "line": diagnostic["range"]["start"]["line"] + 1,
                    "severity": SEVERITY.get(diagnostic.get("severity"), "?"),
                    "code": diagnostic.get("code"),
                    "message": diagnostic["message"],
                }
                rows.append(row)
                out.write(json.dumps(row) + "\n")
                out.flush()
                if row["severity"] == "Error":
                    print(
                        f"  ERROR {relative}:{row['line']} "
                        f"[{row['code']}] {row['message']}",
                        flush=True,
                    )
            if (index + 1) % 100 == 0:
                rate = (time.time() - started) / (index + 1)
                print(f"  ...{index + 1}/{len(files)}  {rate:.2f}s/file", flush=True)
    server.close()

    errors = [r for r in rows if r["severity"] == "Error"]
    print(f"\ndone in {time.time() - started:.0f}s -> {args.out}")
    print(f"{len(errors)} errors across {len({r['file'] for r in errors})} files")
    for code, count in collections.Counter(r["code"] for r in errors).most_common():
        print(f"  {count:5}  {code}")
    print("\nCompare against the project's own compiler output: every error here")
    print("that javac does not also report is a false-positive candidate.")


if __name__ == "__main__":
    main()
