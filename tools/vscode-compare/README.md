# VS Code plugin comparison baseline

Manual, Dockerized baseline that runs an identical scripted probe-and-edit
sequence against the same Maven fixture project **once per flavor, each in
its own container**, from one shared image:

| flavor | container has |
| --- | --- |
| `none` | no extension at all — the measurement floor |
| `ours` | only `java-vsix-lite` |
| `redhat` | only `redhat.java` |

Same image, OS packages, JDK, VS Code build, fixture sources, warm
`~/.m2`, `--shm-size`, settle period, and scripted edits every time — the
single variable is which VSIX is installed.

Separate containers (rather than one container used repeatedly) mean no
flavor inherits another's page cache, JIT state, JVM warmth, or memory
pressure, and the CPU/memory samples taken inside each container describe
that flavor alone. A final throwaway container diffs the reports and renders
the chart, so no measured run is perturbed by post-processing.

This harness is **never run in CI**. It is a manual tool for capturing a
baseline snapshot when comparing the two extensions' behavior; run it by hand
whenever you want a fresh comparison.

## Usage

```sh
tools/vscode-compare/run.sh
```

Prerequisites:

- Docker.
- ~10 GB free disk (VS Code, the JDK base image, Maven + the fixture's
  dependencies, both extensions, and Rust build artifacts).
- Network access during `docker build` only (Node, Rust, VS Code, the
  `redhat.java` VSIX, and the fixture's Maven dependencies). `docker run`
  needs no network: the VS Code build is prefetched and `~/.m2` is warm.

**Settle period.** Each container installs its extension, then waits
`JVL_COMPARE_SETTLE_SECONDS` (default 30) *before* sampling starts and the
probes run, so container creation, image-layer first-touch page faults,
Xvfb boot, and the heavyweight `code --install-extension` Electron run all
land outside the measured window. Set it to `0` for fast iteration when only
the probe values matter:

```sh
JVL_COMPARE_SETTLE_SECONDS=0 tools/vscode-compare/run.sh
```

**Architecture support:** the image builds and runs **natively** on both
`amd64` and `arm64` — the Dockerfile detects the host architecture at build
time (`dpkg --print-architecture`) and downloads the matching pinned Node and
VS Code builds; `vsce` auto-targets the matching platform too. Run
`tools/vscode-compare/run.sh` on an Intel/AMD Linux box or CI runner and on
an Apple Silicon Mac (via Docker Desktop or `colima`) and each produces its
own native-architecture image — no cross-arch emulation is ever attempted.
That matters in practice, not just in principle: running VS Code's Electron
runtime under QEMU user-mode emulation was measured to make a bare
`code --install-extension` grow past 7.6 GB resident memory before the
kernel OOM-killed it, so an emulated run doesn't just run slower, it doesn't
run at all on a typical 16 GB machine.

Output lands in `tools/vscode-compare/out/`:

- `report-<flavor>.json` — raw probe results per flavor, including
  `settleSeconds`, absolute `flavorStartedAt`/`flavorEndedAt`, and per-probe
  `startedAt`/`endedAt` timestamps.
- `comparison.json` — machine-readable diff (`{ summary, rows }`).
- `comparison.md` — human-readable table, with the baseline as a reference
  column and `same`/`differs` judged between the two plugins.
- `stats-<flavor>.csv` — `epochMs,cpuPercent,memMB` rows sampled at 20 Hz
  inside that flavor's own container.
- `timeline.svg` — all three flavors' CPU% and memory overlaid on shared
  axes, clipped to each flavor's measured window (VS Code startup and
  teardown excluded) and marked at each scripted edit.

Pass `--no-cache` through to force a clean image rebuild:

```sh
tools/vscode-compare/run.sh --no-cache
```

## Pinned versions

Read from [`pins.json`](./pins.json):

| artifact | version | verified |
| --- | --- | --- |
| VS Code | `1.137.0` (`linux-x64` or `linux-arm64`, matching the build host) | prefetched at build time |
| Node.js | `20.20.2` | sha256-verified per architecture against the official tarball |
| Rust | `1.98.1` | installed via rustup, used only to build `jvl-server` |
| `redhat.java` (Open VSX) | `1.57.2026090408` | sha256-verified against Open VSX |

The `redhat.java` build is the Open VSX `universal` target (no bundled JRE),
which is why the image installs Eclipse Temurin 21 and the harness points
`java.jdt.ls.java.home` at it.

## The fixture

`tools/vscode-compare/project` is a real Maven project (`demo:orders`), not
a bare source folder: nine classes across records, an enum, an interface and
its implementation, a service layer, plus two ordinary third-party
dependencies (Guava and commons-lang3) that the sources genuinely use
(`ImmutableList`, `ImmutableMap`, `StringUtils`). That matters — a bare
folder only exercises each plugin's syntax tier, while a Maven project with
dependencies exercises project import, classpath resolution, and
generic-aware member lookup across jars, which is where the plugins
actually differ.

`~/.m2/repository` is fully populated at image build time and Maven is
pinned offline (`<offline>true</offline>` in `~/.m2/settings.xml`), so
`docker run` still needs no network and any stray resolution fails fast
instead of hanging.

## Probes

**Capability probes** — one request each, timed:

1. **`server.ready`** — time to first non-empty document symbol response
   (`redhat.java`'s own `serverReady()` API is awaited first when present).
2. **`diagnostics.clean`** — diagnostics on the unmodified fixture.
3. **`symbols.orders`** — the document symbol outline for `Orders.java`.
4. **`hover.total`** — hover contents over `orders.total()` in `Main.java`.
5. **`definition.total`** — go-to-definition target(s) for the same call.
6. **`completion.orders`** — completion items offered after `orders.`.

**Scripted edit workload** — each step makes one realistic change, then
waits for the server to *agree with reality*, so the timing answers "how
long until this plugin reported the right thing", not "how long until it
said something":

7. **`edit.localTypeError`** — `int sum` becomes `String sum`.
8. **`edit.unknownMember`** — call `order.price()`, which does not exist.
9. **`edit.dependencyMisuse`** — call `ImmutableList.copyOfRange(...)`,
   which only a plugin that actually resolved the Guava jar can reject.
10. **`edit.removedImport`** — delete the `ImmutableList` import.
11. **`edit.crossFileRename`** — rename `total()` in `Orders.java`; the
    error must appear in `Main.java`, the file that was *not* edited.
12. **`edit.validAddition`** — add a valid method built from existing API;
    nothing may be reported (catches false positives).
13. **`edit.revertAll`** — restore both files; everything must clear.

Each edit probe waits for an `Error` whose message names the symbol that was
broken, and resets to a verified-clean state between steps. Matching "any
diagnostic appeared" is not good enough: a leftover diagnostic from the
previous edit, or a generic banner like jdt.ls's *"Orders.java is a
non-project file, only syntax errors are reported"*, satisfies that test
instantly and turns "this plugin analyzed nothing" into a row of
impressively fast timings. Both failure modes were hit while building this.

**How the edit probes wait.** They watch `onDidChangeDiagnostics`, not the
clock. Once a server has published for the relevant files *after* the edit
and then stayed quiet for `SETTLE_QUIET_MS` (6 s), its answer is final and
waiting longer cannot change it. Each edit probe records an `outcome`:

| outcome | meaning |
| --- | --- |
| `matched` | reported an error naming the symbol the edit broke |
| `answered-without-match` | analyzed and published, but never flagged it |
| `clean` | analyzed and correctly reported nothing (valid edits) |
| `no-response` | never published at all; ran out the ceiling |

This distinction is worth the machinery. `edit.removedImport` used to consume
the full 120 s ceiling for `java-vsix-lite` and record `value: null` — which
looks identical to a hung server. It now resolves in ~6 s as
`answered-without-match` with the payload attached (an empty set), which is
the *positive* observation that the plugin looked and had nothing to say.
That one change cut the `java-vsix-lite` window from ~122 s to ~13 s.

`SETTLE_QUIET_MS` is deliberately generous relative to the ~2 s worst
observed edit-to-publish latency. Do not tune it down to shave seconds: a
server that publishes a partial result, pauses, then publishes the real one
would be scored `answered-without-match`, silently converting a working
plugin into a fake capability gap. For the same reason `edit.validAddition`
waits for publish-then-quiet rather than sleeping a fixed interval — "stayed
clean" only means something if the server actually looked.

Every probe result records `value`, `timedOut`, `error`, and `elapsedMs`; a
timeout is captured as data (`timedOut: true`), never treated as a harness
failure — the point is observing what each plugin actually does, including
when one is slower or never responds.

**Reading the baseline column:** the `none` flavor installs no extension, so
its capability probes return empty immediately and its edit probes always
time out (at a short 5s window, since nothing will ever answer). Its value
is the resource trace and the wall-clock floor: whatever the container, VS
Code, and this suite cost before any plugin is in the picture.

**A note on version pinning:** the newest `redhat.java` build available at
the time this harness was built (`1.57.2026091108`) throws a repeating
`java.lang.NoSuchFieldError` from `lombok.eclipse.EclipseAST` on essentially
every hover/definition/completion request — a real Lombok-vs-JDT-internals
version-skew regression in that specific nightly build (Lombok patches
Eclipse JDT Core's internal AST classes by reflection, and breaks whenever
JDT Core's internals shift out from under it), confirmed via that build's own
`Language Support for Java.log`, not a bug in this harness. The one-build-older
`1.57.2026090408` does not reproduce it and is what `pins.json` points at —
see the `notes` field there. If you bump `redhatJava` and probes start
timing out again, check that log file (under the VS Code
`--user-data-dir`'s `logs/*/window*/exthost/redhat.java/` before assuming
the harness is at fault; it may be the same class of upstream regression in
whatever new build you picked, in which case pin the prior build instead.

**A note on observed `redhat.java` startup non-determinism:** across
repeated runs against the pinned (working) build, `server.ready` and
`symbols.orders` are bimodal — either both succeed within a few seconds, or
`server.ready` burns the full `READY_TIMEOUT_MS` and `symbols.orders` comes
back empty, while `hover.total`, `definition.total`, `completion.orders`,
and both `diagnostics.*` probes succeed correctly and quickly regardless of
which mode that run landed in. The extension's own
`Language Support for Java.log` shows why: its very first
`textDocument/documentSymbol` request — issued during activation, before
jdt.ls's project model has finished initializing — sometimes gets cancelled
(`Error: Request got cancelled`, thrown from `provideDocumentSymbols`) and,
when that happens, never recovers for that document/session; every other
request handler either doesn't depend on the same not-yet-ready project
model or gets retried successfully. This was checked against host load at
the time (`colima ssh -- uptime`/`free -h` showed the VM essentially idle)
to rule out resource contention as the cause — it reproduces on an
otherwise-quiet host. It is a real upstream race in `redhat.java`, not a
harness bug: nothing here can make the extension's own first request retry
itself, so the honest, correct behavior is exactly what happens today —
record the timeout as data and let every other probe run and report
normally.

## Resource timeline

Each flavor's `runFlavor.js` starts `sampleStats.js`, which reads **its
own** container's cgroup v2 stats (`/sys/fs/cgroup/cpu.stat`'s
`/sys/fs/cgroup/memory.current`) at 20 Hz into `stats-<flavor>.csv`, from a
single long-lived Node process started only after the settle period — a
shell loop forking `awk`/`cat` per sample put its own overhead into the very
counters being measured. Because
the flavors never share a container, those numbers are attributable to one
plugin with no bleed-through from the other. This is deliberately not
`docker stats` polled from the host: such a call takes about a second on its
own, can't usefully be polled faster than 1 Hz, and would measure whichever
container happened to be alive rather than a named flavor.

`run.sh` then renders `timeline.svg` in a throwaway container (reusing the
image's own pinned Node, so this needs no host Node install). The two runs
have unrelated wall clocks, so both series are plotted against **elapsed
time since their own container started** and overlaid on shared axes — blue
for `java-vsix-lite`, orange for `redhat.java`, one panel for CPU and one
for memory — so reading straight down any x position compares the two
plugins at the same point in their own lifetime. Dashed vertical markers,
colored per flavor, sit at VS Code readiness and at the scripted
edit/revert moments specifically; read-only probes like hover or completion
aren't marked, since they aren't "something changed".

The time axis is compressed with a fisheye transform: every marker keeps a
few seconds of full-resolution context on each side, and any idle stretch
further from a marker than that collapses toward a few seconds of display
width regardless of how long it really lasted (labeled
`⋯ Nm SSs idle, compressed ⋯` at the seam). Without this, the multi-minute
`server.ready` stall described above would — as it did during development —
squeeze every marker, including the two the graph exists to show, into a
few illegible pixels at the edges of an otherwise-empty chart.

## Upgrade procedure

1. Bump the version fields in `pins.json`.
2. Update the matching `ARG` defaults in `Dockerfile` (`NODE_VERSION`,
   `RUST_VERSION`, `VSCODE_VERSION`, `REDHAT_JAVA_URL`).
3. Refresh `redhatJava.sha256`: Open VSX publishes a `.sha256` sidecar next to
   every VSIX —
   `curl -fsSL https://open-vsx.org/api/redhat/java/<version>/file/redhat.java-<version>.sha256`
   — and copy the result into both `pins.json` and the `REDHAT_JAVA_SHA256`
   `ARG`.
4. Refresh both `nodeSha256Amd64` and `nodeSha256Arm64` from
   `https://nodejs.org/dist/v<version>/SHASUMS256.txt` (grep both
   `linux-x64` and `linux-arm64` lines) — the image needs whichever one
   matches the machine it's built on.
5. Re-run `tools/vscode-compare/run.sh` and, if you want a recorded baseline,
   commit the new `out/comparison.md` (and `out/timeline.svg` if useful).
