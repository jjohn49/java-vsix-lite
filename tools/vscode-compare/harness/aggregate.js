// Aggregates N repetitions of the comparison into one report carrying a
// sample size and a spread, so a claim like "ours answers this edit in 40ms"
// is backed by a distribution rather than a single observation.
//
// Reads `<out>/run-*/` (each the output of one full three-container
// repetition) and writes `aggregate.json` + `aggregate.md` at the top level.
//
// Reporting choices, all deliberate:
//   * Median, not mean, is the headline. These are latency samples on a
//     machine that occasionally does something else; one descheduled probe
//     drags a mean and leaves a median alone.
//   * Min/max are printed in full rather than reduced to an error bar. With
//     the handful of repetitions this tool is built for, the actual range is
//     more honest than a standard deviation implying a normal distribution
//     that latency does not have.
//   * Stddev is still emitted in the JSON for anyone who wants it, and is
//     null at n=1 rather than 0 -- one sample has no dispersion, and
//     printing 0 would claim perfect reproducibility.
//   * Every per-run value is listed, so a cold first run or a single outlier
//     stays visible instead of being smoothed into the summary.
const fs = require("node:fs");
const path = require("node:path");

const OUT_DIR = process.argv[2] ?? process.env.JVL_COMPARE_OUT_DIR ?? "/out";
const FLAVORS = ["none", "ours", "redhat"];
const FLAVOR_LABEL = {
  none: "baseline",
  ours: "java-vsix-lite",
  redhat: "redhat.java",
};

function readJson(file) {
  try {
    return JSON.parse(fs.readFileSync(file, "utf8"));
  } catch {
    return null;
  }
}

const env = readJson(path.join(OUT_DIR, "environment.json")) ?? {};
// The sampler reports CPU as a percentage of one core, so the ceiling for a
// machine offering N cores is N*100. Unknown core count disables saturation
// reporting rather than guessing a ceiling and mislabelling a healthy run.
const CPU_CEILING_PERCENT = Number(env.dockerCpus) > 0 ? Number(env.dockerCpus) * 100 : null;

const runDirs = fs
  .readdirSync(OUT_DIR, { withFileTypes: true })
  .filter((d) => d.isDirectory() && /^run-\d+$/.test(d.name))
  .map((d) => path.join(OUT_DIR, d.name))
  .sort();

if (runDirs.length === 0) {
  console.error(`no run-* directories under ${OUT_DIR}`);
  process.exit(1);
}

// ---- statistics ------------------------------------------------------

function median(values) {
  if (values.length === 0) return null;
  const s = [...values].sort((a, b) => a - b);
  const mid = s.length >> 1;
  return s.length % 2 ? s[mid] : (s[mid - 1] + s[mid]) / 2;
}

function mean(values) {
  return values.length === 0 ? null : values.reduce((a, b) => a + b, 0) / values.length;
}

/** Sample standard deviation (n-1). Null below two samples: one observation
 *  has no spread, and reporting 0 would assert reproducibility never tested. */
function stddev(values) {
  if (values.length < 2) return null;
  const m = mean(values);
  return Math.sqrt(values.reduce((acc, v) => acc + (v - m) ** 2, 0) / (values.length - 1));
}

function describe(values) {
  const clean = values.filter((v) => typeof v === "number" && Number.isFinite(v));
  if (clean.length === 0) return null;
  return {
    n: clean.length,
    median: median(clean),
    mean: mean(clean),
    stddev: stddev(clean),
    min: Math.min(...clean),
    max: Math.max(...clean),
    values: clean,
  };
}

const round = (v, digits = 1) =>
  v === null || v === undefined ? null : Number(v.toFixed(digits));

// ---- resource cost per run ------------------------------------------

/**
 * Integrate the sampler's CPU% trace into core-seconds actually consumed.
 *
 * This is the metric worth comparing. Peak CPU% only says how wide a plugin
 * spread itself across cores for one instant, and mean CPU% is an average
 * over a window whose length is itself one of the things that differs
 * between flavors -- a plugin that finishes in 3s at 300% looks "worse" than
 * one that takes 30s at 80% by that measure, while having done a tenth of
 * the work. Core-seconds is the invariant: total CPU time billed.
 */
function costOf(statsFile) {
  let text;
  try {
    text = fs.readFileSync(statsFile, "utf8");
  } catch {
    return null;
  }
  const rows = text
    .split("\n")
    .filter(Boolean)
    .map((line) => line.split(",").map(Number))
    .filter((r) => r.length >= 3 && r.every(Number.isFinite));
  if (rows.length < 2) return null;

  let coreSeconds = 0;
  for (let i = 1; i < rows.length; i++) {
    const dtMs = rows[i][0] - rows[i - 1][0];
    if (dtMs <= 0) continue;
    coreSeconds += (rows[i][1] / 100) * (dtMs / 1000);
  }
  // Share of samples pinned at the machine's CPU ceiling. When this is
  // non-trivial the run stopped measuring the plugin and started measuring
  // the box: demand above the ceiling is invisible, wall-clock stretches to
  // absorb it, and core-seconds is truncated. A comparison where one flavor
  // saturates and the other does not is not a fair one, so it gets said out
  // loud rather than left for a reader to infer from the timeline.
  const ceiling = CPU_CEILING_PERCENT;
  const saturated = ceiling
    ? rows.filter((r) => r[1] >= ceiling * 0.95).length / rows.length
    : null;
  return {
    coreSeconds,
    peakCpuPercent: Math.max(...rows.map((r) => r[1])),
    saturatedFraction: saturated,
    peakMemMB: Math.max(...rows.map((r) => r[2])),
    meanMemMB: mean(rows.map((r) => r[2])),
    samples: rows.length,
  };
}

// ---- collect ---------------------------------------------------------

const runs = runDirs.map((dir) => ({
  dir: path.basename(dir),
  meta: readJson(path.join(dir, "run-meta.json")),
  reports: Object.fromEntries(
    FLAVORS.map((f) => [f, readJson(path.join(dir, `report-${f}.json`))]),
  ),
  cost: Object.fromEntries(FLAVORS.map((f) => [f, costOf(path.join(dir, `stats-${f}.csv`))])),
}));

const probeIds = [
  ...new Set(
    runs.flatMap((r) => FLAVORS.flatMap((f) => (r.reports[f]?.probes ?? []).map((p) => p.id))),
  ),
].sort();

const perFlavor = Object.fromEntries(
  FLAVORS.map((flavor) => {
    const windows = runs.map((r) => {
      const rep = r.reports[flavor];
      return rep?.flavorStartedAt && rep?.flavorEndedAt
        ? rep.flavorEndedAt - rep.flavorStartedAt
        : null;
    });
    return [
      flavor,
      {
        windowMs: describe(windows),
        coreSeconds: describe(runs.map((r) => r.cost[flavor]?.coreSeconds ?? null)),
        peakMemMB: describe(runs.map((r) => r.cost[flavor]?.peakMemMB ?? null)),
        meanMemMB: describe(runs.map((r) => r.cost[flavor]?.meanMemMB ?? null)),
        saturatedPct: describe(
          runs.map((r) =>
            r.cost[flavor]?.saturatedFraction === null ||
            r.cost[flavor]?.saturatedFraction === undefined
              ? null
              : r.cost[flavor].saturatedFraction * 100,
          ),
        ),
        retries: runs.reduce((acc, r) => acc + ((r.meta?.attempts?.[flavor] ?? 1) - 1), 0),
      },
    ];
  }),
);

const probes = probeIds.map((id) => {
  const byFlavor = Object.fromEntries(
    FLAVORS.map((flavor) => {
      const hits = runs.map((r) => (r.reports[flavor]?.probes ?? []).find((p) => p.id === id));
      // Distinct results across runs. A probe whose *value* changes between
      // repetitions is not a timing outlier, it is non-deterministic
      // behaviour, and that matters more than any percentile below it.
      const signatures = new Set(
        hits.filter(Boolean).map((p) => JSON.stringify(p.timedOut ? "<timed out>" : p.value)),
      );
      return [
        flavor,
        {
          elapsedMs: describe(hits.map((p) => (p?.timedOut ? null : (p?.elapsedMs ?? null)))),
          timedOutRuns: hits.filter((p) => p?.timedOut).length,
          distinctResults: signatures.size,
          outcomes: [
            ...new Set(hits.map((p) => p?.value?.outcome).filter(Boolean)),
          ].sort(),
        },
      ];
    }),
  );
  return { id, byFlavor };
});

// ---- render ----------------------------------------------------------


const n = runs.length;
const fmt = (d) =>
  d === null
    ? "—"
    : d.n === 1
      ? `${round(d.median)}`
      : `${round(d.median)} [${round(d.min)}–${round(d.max)}]`;

const lines = [
  `# java-vsix-lite vs redhat.java — aggregate of ${n} run${n === 1 ? "" : "s"}`,
  "",
  `- host: \`${env.host ?? "unknown"}\``,
  `- docker: \`${env.dockerServerVersion ?? "?"}\`, ${env.dockerCpus ?? "?"} CPUs available to containers`,
  `- image: \`${(env.imageId ?? "").slice(0, 19)}\` (identical across all runs)`,
  `- settle before sampling: ${env.settleSeconds ?? "?"}s`,
  `- runs: ${n}${n < 5 ? " — small sample; treat the range, not the median, as the result" : ""}`,
  "",
  "Cells show **median [min–max]** across runs. A single number means n=1.",
  "",
  "## Cost per flavor",
  "",
  "`core-s` is CPU time actually consumed over the measured window (the",
  "integral of the sampler's CPU% trace), which is comparable across flavors",
  "whose windows differ in length — unlike peak or mean CPU%.",
  "",
  "| flavor | window (ms) | CPU (core-s) | peak mem (MB) | CPU-capped | retries |",
  "| --- | --- | --- | --- | --- | --- |",
];
for (const flavor of FLAVORS) {
  const f = perFlavor[flavor];
  lines.push(
    `| ${FLAVOR_LABEL[flavor]} | ${fmt(f.windowMs)} | ${fmt(f.coreSeconds)} | ${fmt(f.peakMemMB)} | ` +
      `${f.saturatedPct ? `${round(f.saturatedPct.median)}%` : "—"} | ${f.retries} |`,
  );
}

// A saturated flavor turns the comparison into a measurement of the host.
// Say so at the top of the report, where it cannot be missed, including the
// direction of the resulting bias.
const capped = FLAVORS.filter((f) => (perFlavor[f].saturatedPct?.median ?? 0) >= 5);
if (capped.length > 0) {
  lines.push(
    "",
    `> **CPU-capped: ${capped.map((f) => FLAVOR_LABEL[f]).join(", ")}.** ` +
      `This machine offers ${env.dockerCpus} CPUs to containers, and the flavor(s) above ` +
      "spent a material share of the measured window pinned at that ceiling. Demand " +
      "beyond it is invisible: wall-clock stretches to absorb the excess and core-seconds " +
      "is truncated, so those figures are lower bounds on cost and upper bounds on speed. " +
      "Re-run on a host with more cores before quoting these numbers.",
  );
}

lines.push(
  "",
  "## Probe latency",
  "",
  "`n` counts runs that produced a timing; a probe that timed out contributes",
  "no latency, so its `n` is lower and its timeout count is shown instead.",
  "",
  "| probe | java-vsix-lite (ms) | redhat.java (ms) | baseline (ms) | stable? |",
  "| --- | --- | --- | --- | --- |",
);
for (const probe of probes) {
  const cell = (flavor) => {
    const p = probe.byFlavor[flavor];
    const base = fmt(p.elapsedMs);
    if (p.timedOutRuns > 0) {
      return `${base} (${p.timedOutRuns}× timed out)`;
    }
    // A `no-response` probe still reports an elapsed time -- the window it
    // waited out. Printing that bare reads as "answered in 5.8s", which is
    // the opposite of what happened, so label it.
    if (p.outcomes.length > 0 && p.outcomes.every((o) => o === "no-response")) {
      return `${base} (no response)`;
    }
    return base;
  };
  // "Stable" means both plugins returned an identical result in every run.
  // Unstable is the interesting case: it says the comparison itself is
  // sampling a moving target, and no amount of repetition fixes that.
  const unstable = ["ours", "redhat"].filter((f) => probe.byFlavor[f].distinctResults > 1);
  const stable = unstable.length === 0 ? "yes" : `no (${unstable.join(", ")})`;
  lines.push(
    `| ${probe.id} | ${cell("ours")} | ${cell("redhat")} | ${cell("none")} | ${stable} |`,
  );
}

const ours = perFlavor.ours;
const redhat = perFlavor.redhat;
if (ours.coreSeconds && redhat.coreSeconds) {
  lines.push(
    "",
    "## Headline",
    "",
    `Over ${n} run${n === 1 ? "" : "s"}, java-vsix-lite consumed a median of ` +
      `${round(ours.coreSeconds.median, 2)} core-seconds against redhat.java's ` +
      `${round(redhat.coreSeconds.median, 2)} ` +
      `(${round(redhat.coreSeconds.median / ours.coreSeconds.median, 1)}×), ` +
      `peaking at ${round(ours.peakMemMB.median)} MB against ` +
      `${round(redhat.peakMemMB.median)} MB ` +
      `(${round(redhat.peakMemMB.median / ours.peakMemMB.median, 1)}×).`,
  );
}

fs.writeFileSync(
  path.join(OUT_DIR, "aggregate.json"),
  JSON.stringify({ environment: env, runs: runs.map((r) => r.meta), perFlavor, probes }, null, 2),
);
fs.writeFileSync(path.join(OUT_DIR, "aggregate.md"), `${lines.join("\n")}\n`);
console.log(`wrote ${path.join(OUT_DIR, "aggregate.md")} from ${n} run(s)`);
