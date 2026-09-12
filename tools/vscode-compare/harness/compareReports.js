// Diffs the flavor reports into comparison.json + comparison.md. The
// no-extension baseline is shown as context, not as a comparison target:
// "same/differs" is judged between java-vsix-lite and redhat.java, while the
// baseline column says what the container, VS Code, and this suite produce
// with no Java support at all. Exit status is always 0 — this is a baseline
// capture, not a pass/fail gate.
const fs = require("node:fs");
const path = require("node:path");

const OUT_DIR = process.argv[2] ?? process.env.JVL_COMPARE_OUT_DIR ?? "/out";
const FLAVORS = ["none", "ours", "redhat"];
const FLAVOR_HEADING = {
  none: "baseline (no extension)",
  ours: "java-vsix-lite",
  redhat: "redhat.java",
};

function loadReport(flavor) {
  const file = path.join(OUT_DIR, `report-${flavor}.json`);
  if (!fs.existsSync(file)) {
    return null;
  }
  try {
    return JSON.parse(fs.readFileSync(file, "utf8"));
  } catch {
    return null;
  }
}

const reports = Object.fromEntries(FLAVORS.map((f) => [f, loadReport(f)]));
const probesById = Object.fromEntries(
  FLAVORS.map((f) => [f, new Map((reports[f]?.probes ?? []).map((p) => [p.id, p]))]),
);
const ids = [...new Set(FLAVORS.flatMap((f) => [...probesById[f].keys()]))].sort();

function render(probe) {
  if (!probe) {
    return "_absent_";
  }
  if (probe.error) {
    return `error: ${probe.error}`;
  }
  if (probe.timedOut) {
    return "_timed out_";
  }
  const value = JSON.stringify(probe.value);
  const clipped = value.length > 300 ? `${value.slice(0, 300)}...` : value;
  return `\`${clipped.replace(/\|/g, "\\|")}\``;
}

const rows = ids.map((id) => {
  const byFlavor = Object.fromEntries(FLAVORS.map((f) => [f, probesById[f].get(id) ?? null]));
  const same =
    JSON.stringify(byFlavor.ours?.value ?? null) === JSON.stringify(byFlavor.redhat?.value ?? null);
  return { id, ...byFlavor, same };
});

const summary = {
  identical: rows.filter((r) => r.same).length,
  different: rows.filter((r) => !r.same).length,
  total: rows.length,
  totalElapsedMs: Object.fromEntries(
    FLAVORS.map((f) => [
      f,
      reports[f]?.flavorEndedAt && reports[f]?.flavorStartedAt
        ? reports[f].flavorEndedAt - reports[f].flavorStartedAt
        : null,
    ]),
  ),
};

const lines = [
  "# java-vsix-lite vs redhat.java — baseline comparison",
  "",
  `- VS Code: \`${reports.ours?.vscodeVersion ?? reports.redhat?.vscodeVersion ?? "?"}\``,
  `- java-vsix-lite: \`${reports.ours?.extensionVersion ?? "absent"}\``,
  `- redhat.java: \`${reports.redhat?.extensionVersion ?? "absent"}\``,
  `- baseline: no extension installed (container + VS Code + probe suite only)`,
  `- settle before sampling: ${reports.ours?.settleSeconds ?? "?"}s`,
  `- probes: ${summary.total} (ours vs redhat — identical ${summary.identical}, different ${summary.different})`,
  "",
  "## Measured window per flavor",
  "",
  "| flavor | probe-run duration |",
  "| --- | --- |",
  ...FLAVORS.map(
    (f) =>
      `| ${FLAVOR_HEADING[f]} | ${
        summary.totalElapsedMs[f] === null ? "_absent_" : `${summary.totalElapsedMs[f]} ms`
      } |`,
  ),
  "",
  "## Probes",
  "",
  "| probe | ours vs redhat | baseline (ms) | java-vsix-lite (ms) | redhat.java (ms) | java-vsix-lite | redhat.java |",
  "| --- | --- | --- | --- | --- | --- | --- |",
];
for (const row of rows) {
  lines.push(
    `| ${row.id} | ${row.same ? "same" : "differs"} | ${row.none?.elapsedMs ?? "-"} | ${
      row.ours?.elapsedMs ?? "-"
    } | ${row.redhat?.elapsedMs ?? "-"} | ${render(row.ours)} | ${render(row.redhat)} |`,
  );
}

fs.writeFileSync(
  path.join(OUT_DIR, "comparison.json"),
  JSON.stringify({ summary, rows }, null, 2),
);
fs.writeFileSync(path.join(OUT_DIR, "comparison.md"), `${lines.join("\n")}\n`);
console.log(`wrote ${path.join(OUT_DIR, "comparison.md")}`);
console.log(JSON.stringify(summary));
