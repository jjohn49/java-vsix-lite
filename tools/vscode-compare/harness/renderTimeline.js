// Renders an SVG comparing container CPU% and memory between the two
// flavors. Each flavor runs in its own container, so their wall clocks are
// unrelated; both series are therefore plotted against *elapsed time since
// that flavor's own container started*, overlaid on shared axes — at any x
// you are comparing the two plugins at the same point in their own lifetime.
// Reads stats-ours.csv / stats-redhat.csv (high-frequency cgroup samplers
// written by entrypoint.sh inside each container) plus report-ours.json /
// report-redhat.json for event timestamps. Never throws on missing or
// partial data — the graph is a bonus on top of the JSON/MD reports, not a
// gate on them.
const fs = require("node:fs");
const path = require("node:path");

const OUT_DIR = process.env.JVL_COMPARE_OUT_DIR || "/out";
const WIDTH = 1200;
const MARGIN = { top: 96, right: 100, bottom: 44, left: 64 };
const CPU_HEIGHT = 210;
const MEM_HEIGHT = 210;
const CHART_GAP = 96; // room for stacked, rotated marker labels between panels
const PLOT_WIDTH = WIDTH - MARGIN.left - MARGIN.right;
// Panels are laid out per-render (the label rows set the gap), so overall
// height is computed in renderSvg rather than fixed here.

const FLAVORS = ["none", "ours", "redhat"];
const FLAVOR_COLOR = { none: "#6b7280", ours: "#2563eb", redhat: "#ea580c" };
const FLAVOR_LABEL = { none: "no extension (baseline)", ours: "java-vsix-lite", redhat: "redhat.java" };
// Only these probe ids are meaningful "something changed" moments; the
// read-only probes (hover, definition, completion, symbols) would just
// clutter the timeline.
const KEY_PROBE_LABELS = {
  "edit.localTypeError": "type error",
  "edit.unknownMember": "unknown member",
  "edit.dependencyMisuse": "dependency misuse",
  "edit.removedImport": "import removed",
  "edit.crossFileRename": "cross-file rename",
  "edit.validAddition": "valid addition",
  "edit.revertAll": "revert all",
};

// Full display resolution is kept within this much elapsed time of every
// event, at the start/end, and across every stretch where either flavor is
// actually burning CPU. Only what's left — far from a marker *and* quiet in
// both series — collapses toward IDLE_COMPRESS_MS, so a long stall can't
// squeeze the markers into an illegible sliver and real work can never be
// hidden.
const CONTEXT_MS = 3000;
const IDLE_COMPRESS_MS = 3000;
// Don't bother compressing a quiet stretch shorter than this: collapsing
// every few-second lull turns the chart into a row of break marks that cost
// more legibility than the space they recover.
const MIN_COMPRESS_MS = 8000;
// A sample at or above this share of one core counts as work, not idle.
const BUSY_CPU_PERCENT = 20;
const BUSY_WINDOW_MS = 400;

/** Round up to a "nice" gridline maximum (1/2/5 × a power of ten). */
function niceCeil(value) {
  if (value <= 0) {
    return 1;
  }
  const magnitude = 10 ** Math.floor(Math.log10(value));
  for (const step of [1, 2, 2.5, 5, 10]) {
    const candidate = step * magnitude;
    if (candidate >= value) {
      return candidate;
    }
  }
  return 10 * magnitude;
}

/** stats-<flavor>.csv rows are `epochMs,cpuPercent,memMB` — plain numbers. */
function loadStats(flavor) {
  const file = path.join(OUT_DIR, `stats-${flavor}.csv`);
  if (!fs.existsSync(file)) {
    return [];
  }
  const points = [];
  for (const line of fs.readFileSync(file, "utf8").split("\n")) {
    const trimmed = line.trim();
    if (!trimmed) {
      continue;
    }
    const [tsRaw, cpuRaw, memRaw] = trimmed.split(",");
    const ts = parseInt(tsRaw, 10);
    const cpu = parseFloat(cpuRaw);
    const mem = parseFloat(memRaw);
    if (Number.isFinite(ts) && Number.isFinite(cpu) && Number.isFinite(mem)) {
      points.push({ ts, cpu, mem });
    }
  }
  return points.sort((a, b) => a.ts - b.ts);
}

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

/** The flavor's start plus its labeled state-change probes (edit/revert). */
function collectEvents(flavor, report) {
  if (!report) {
    return [];
  }
  const events = [];
  if (report.flavorStartedAt) {
    events.push({ ts: report.flavorStartedAt, flavor, label: "VS Code ready" });
  }
  for (const probe of report.probes ?? []) {
    const label = KEY_PROBE_LABELS[probe.id];
    if (label && probe.startedAt) {
      events.push({ ts: probe.startedAt, flavor, label });
    }
  }
  return events;
}

/**
 * Scan the elapsed-time range on a coarse grid and return the windows where
 * either flavor is consuming CPU, so they can be excluded from compression.
 */
function busyWindows(maxElapsed, isBusyAt) {
  const STEP_MS = 250;
  const windows = [];
  let openedAt = null;
  for (let t = 0; t <= maxElapsed; t += STEP_MS) {
    if (isBusyAt(t)) {
      if (openedAt === null) {
        openedAt = t;
      }
    } else if (openedAt !== null) {
      windows.push([openedAt, t]);
      openedAt = null;
    }
  }
  if (openedAt !== null) {
    windows.push([openedAt, maxElapsed]);
  }
  return windows;
}

/**
 * A "fisheye" elapsed-time -> display-time mapping shared by both series:
 * full resolution near any event *and* wherever either flavor is actually
 * doing work; only stretches that are both far from a marker and quiet in
 * both series get compressed.
 *
 * The quiet test is load-bearing. Compressing purely by distance-from-a-
 * marker hid real work: `redhat.java` spends seconds between its readiness
 * and its edit markers pegging 300%+ of a core indexing, and an
 * event-distance-only rule collapsed exactly that stretch and labeled it
 * "idle". A graph that hides the busiest part of a run is worse than no
 * graph.
 */
function buildTimeMapper(maxElapsed, eventOffsets, isBusyAt) {
  if (maxElapsed <= 0) {
    return { toDisplay: () => 0, totalDisplay: 1, gaps: [] };
  }
  const raw = eventOffsets.map((ms) => [
    Math.max(0, ms - CONTEXT_MS),
    Math.min(maxElapsed, ms + CONTEXT_MS),
  ]);
  raw.push([0, Math.min(maxElapsed, CONTEXT_MS)]);
  raw.push([Math.max(0, maxElapsed - CONTEXT_MS), maxElapsed]);
  // Every busy window stays at full resolution too.
  for (const [s, e] of busyWindows(maxElapsed, isBusyAt)) {
    raw.push([s, e]);
  }
  raw.sort((a, b) => a[0] - b[0]);
  const active = [];
  for (const [s, e] of raw) {
    const last = active[active.length - 1];
    if (last && s <= last[1]) {
      last[1] = Math.max(last[1], e);
    } else {
      active.push([s, e]);
    }
  }

  const segments = [];
  let cum = 0;
  let cursor = 0;
  const pushQuiet = (start, end) => {
    const realMs = end - start;
    const compressed = realMs > MIN_COMPRESS_MS;
    const dur = compressed ? IDLE_COMPRESS_MS : realMs;
    segments.push({ realStart: start, realEnd: end, dispStart: cum, dispEnd: cum + dur, compressed, realMs });
    cum += dur;
  };
  for (const [s, e] of active) {
    if (s > cursor) {
      pushQuiet(cursor, s);
    }
    const dur = e - s;
    segments.push({ realStart: s, realEnd: e, dispStart: cum, dispEnd: cum + dur, compressed: false });
    cum += dur;
    cursor = Math.max(cursor, e);
  }
  if (cursor < maxElapsed) {
    pushQuiet(cursor, maxElapsed);
  }
  const totalDisplay = Math.max(1, cum);

  function toDisplay(ms) {
    if (ms <= 0) {
      return 0;
    }
    if (ms >= maxElapsed) {
      return totalDisplay;
    }
    for (const seg of segments) {
      if (ms >= seg.realStart && ms <= seg.realEnd) {
        const frac = seg.realEnd > seg.realStart ? (ms - seg.realStart) / (seg.realEnd - seg.realStart) : 0;
        return seg.dispStart + frac * (seg.dispEnd - seg.dispStart);
      }
    }
    return totalDisplay;
  }

  return { toDisplay, totalDisplay, gaps: segments.filter((s) => s.compressed && s.realMs > 0) };
}

function esc(text) {
  return String(text).replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" })[c]);
}

function svgMessage(text) {
  return (
    `<svg xmlns="http://www.w3.org/2000/svg" width="${WIDTH}" height="200">` +
    `<rect width="100%" height="100%" fill="#ffffff"/>` +
    `<text x="20" y="100" font-family="sans-serif" font-size="16" fill="#374151">${esc(text)}</text>` +
    `</svg>\n`
  );
}

function formatDuration(ms) {
  const totalSeconds = Math.round(ms / 1000);
  if (totalSeconds < 60) {
    return `${totalSeconds}s`;
  }
  return `${Math.floor(totalSeconds / 60)}m${String(totalSeconds % 60).padStart(2, "0")}s`;
}

/** Axis, gridlines, and one path per flavor, at a given vertical offset. */
function renderPanel({ series, key, yTop, height, maxY, unit, title, toX }) {
  const y = (v) => yTop + height - (Math.min(v, maxY) / maxY) * height;

  const gridLines = [0, 0.25, 0.5, 0.75, 1].map((frac) => {
    const gy = yTop + height - frac * height;
    return (
      `<line x1="${MARGIN.left}" y1="${gy.toFixed(1)}" x2="${MARGIN.left + PLOT_WIDTH}" y2="${gy.toFixed(1)}" stroke="#e5e7eb" stroke-width="1"/>` +
      `<text x="${MARGIN.left - 8}" y="${(gy + 4).toFixed(1)}" font-family="sans-serif" font-size="11" fill="#6b7280" text-anchor="end">${Math.round(maxY * frac)}${unit}</text>`
    );
  });

  const paths = FLAVORS.filter((f) => (series[f] ?? []).length > 1).map((flavor) => {
    const d = series[flavor]
      .map((p, i) => `${i === 0 ? "M" : "L"} ${toX(p.elapsed).toFixed(1)} ${y(p[key]).toFixed(1)}`)
      .join(" ");
    return `<path d="${d}" fill="none" stroke="${FLAVOR_COLOR[flavor]}" stroke-width="1.6" opacity="0.9"/>`;
  });

  return (
    `<text x="${MARGIN.left}" y="${yTop - 10}" font-family="sans-serif" font-size="13" font-weight="600" fill="#111827">${esc(title)}</text>` +
    `<rect x="${MARGIN.left}" y="${yTop}" width="${PLOT_WIDTH}" height="${height}" fill="#fafafa" stroke="#d1d5db"/>` +
    gridLines.join("") +
    paths.join("")
  );
}

function renderSvg(series, events, durations) {
  const allPoints = FLAVORS.flatMap((f) => series[f] ?? []);
  const maxElapsed = Math.max(1, ...allPoints.map((p) => p.elapsed), ...events.map((e) => e.elapsed));

  // "Busy" at time t = either flavor had a sample near t above the CPU
  // floor. Only genuinely quiet stretches are eligible for compression.
  const isBusyAt = (t) =>
    allPoints.some((p) => Math.abs(p.elapsed - t) <= BUSY_WINDOW_MS && p.cpu >= BUSY_CPU_PERCENT);

  const { toDisplay, totalDisplay, gaps } = buildTimeMapper(
    maxElapsed,
    events.map((e) => e.elapsed),
    isBusyAt,
  );
  const toX = (ms) => MARGIN.left + (toDisplay(ms) / totalDisplay) * PLOT_WIDTH;

  // Labels are laid out before the panels, because the number of rows they
  // stack into determines how much room the panels must leave between them.
  // A fixed gap silently broke once java-vsix-lite got fast enough to fire
  // five markers inside a few hundred pixels: the bottom row overprinted the
  // memory panel's title.
  const LABEL_ROW_HEIGHT = 13;
  const CHAR_WIDTH = 5.4; // ~10px sans-serif
  const LABEL_TOP_PAD = 16;
  const placed = [];
  const labelLayout = events.map((e) => {
    const x = toX(e.elapsed);
    const width = e.label.length * CHAR_WIDTH + 10;
    let row = 0;
    while (placed.some((p) => p.row === row && x < p.end && x + width > p.start)) {
      row += 1;
    }
    placed.push({ row, start: x, end: x + width });
    return { event: e, x, row };
  });
  const labelRows = placed.length > 0 ? Math.max(...placed.map((p) => p.row)) + 1 : 0;
  const chartGap = Math.max(CHART_GAP, LABEL_TOP_PAD + labelRows * LABEL_ROW_HEIGHT + 18);

  const cpuTop = MARGIN.top;
  const memTop = MARGIN.top + CPU_HEIGHT + chartGap;
  const panelBottom = memTop + MEM_HEIGHT;
  const maxCpu = niceCeil(Math.max(1, ...allPoints.map((p) => p.cpu)) * 1.05);
  const maxMem = niceCeil(Math.max(1, ...allPoints.map((p) => p.mem)) * 1.1);

  const breakMarks = gaps
    .map((g) => {
      const gx0 = MARGIN.left + (g.dispStart / totalDisplay) * PLOT_WIDTH;
      const gx1 = MARGIN.left + (g.dispEnd / totalDisplay) * PLOT_WIDTH;
      const gxMid = (gx0 + gx1) / 2;
      const midY = cpuTop + CPU_HEIGHT / 2;
      return (
        `<rect x="${gx0.toFixed(1)}" y="${MARGIN.top - 4}" width="${Math.max(1, gx1 - gx0).toFixed(1)}" ` +
        `height="${panelBottom - (MARGIN.top - 4) + 4}" fill="#ffffff" stroke="#d1d5db" stroke-width="0.5"/>` +
        `<text x="${gxMid.toFixed(1)}" y="${midY.toFixed(1)}" font-family="sans-serif" font-size="9" fill="#9ca3af" ` +
        `text-anchor="middle" transform="rotate(-90 ${gxMid.toFixed(1)} ${midY.toFixed(1)})">` +
        `⋯ ${esc(formatDuration(g.realMs))} quiet (&lt;${BUSY_CPU_PERCENT}% CPU), compressed ⋯</text>`
      );
    })
    .join("");

  // Two segments per marker, one per panel: a single full-height line would
  // strike through the label band and the memory panel's title. The band is
  // left to the labels and their leaders.
  const markerLines = events
    .map((e) => {
      const cx = toX(e.elapsed).toFixed(1);
      const stroke =
        `stroke="${FLAVOR_COLOR[e.flavor]}" stroke-width="2" stroke-dasharray="5,3" opacity="0.9"`;
      return (
        `<line x1="${cx}" y1="${MARGIN.top - 4}" x2="${cx}" y2="${cpuTop + CPU_HEIGHT}" ${stroke}/>` +
        `<line x1="${cx}" y1="${memTop}" x2="${cx}" y2="${panelBottom}" ${stroke}/>`
      );
    })
    .join("");

  // Horizontal labels stacked into rows, each tied back to its marker by a
  // thin leader. Rotated labels were unreadable here: java-vsix-lite fires
  // all of its events inside a fraction of a multi-second axis, so several
  // markers land within a few dozen pixels and diagonal text smears across
  // itself. Row assignment happened above, where it sized the panel gap.
  const baseLabelY = cpuTop + CPU_HEIGHT + LABEL_TOP_PAD;
  const markerLabels = labelLayout
    .map(({ event, x, row }) => {
      const cx = x.toFixed(1);
      const y = baseLabelY + row * LABEL_ROW_HEIGHT;
      return (
        `<line x1="${cx}" y1="${(cpuTop + CPU_HEIGHT).toFixed(1)}" x2="${cx}" y2="${(y - 8).toFixed(1)}" ` +
        `stroke="${FLAVOR_COLOR[event.flavor]}" stroke-width="1" opacity="0.5"/>` +
        `<text x="${(x + 4).toFixed(1)}" y="${y.toFixed(1)}" font-family="sans-serif" font-size="10" ` +
        `font-weight="600" fill="${FLAVOR_COLOR[event.flavor]}">${esc(event.label)}</text>`
      );
    })
    .join("");

  let legendX = MARGIN.left;
  const legend = FLAVORS.map((flavor) => {
    const summary = durations[flavor]
      ? ` — ${formatDuration(durations[flavor].elapsed)}, ${durations[flavor].samples} samples`
      : " — no data";
    const text = FLAVOR_LABEL[flavor] + summary;
    const cx = legendX;
    // ~6.2px per character at 12px sans-serif, plus the swatch and a gap;
    // a fixed step overlapped once the baseline label was added.
    legendX += 22 + text.length * 6.2 + 24;
    return (
      `<circle cx="${cx}" cy="26" r="5" fill="${FLAVOR_COLOR[flavor]}"/>` +
      `<text x="${cx + 12}" y="30" font-family="sans-serif" font-size="12" fill="#111827">${esc(text)}</text>`
    );
  }).join("");

  const height = panelBottom + MARGIN.bottom;
  const compressedNote = gaps.length > 0 ? `, ${gaps.length} quiet stretch(es) compressed` : "";
  const xAxisLabel =
    `<text x="${MARGIN.left + PLOT_WIDTH / 2}" y="${height - 10}" font-family="sans-serif" font-size="11" fill="#6b7280" ` +
    `text-anchor="middle">elapsed time within each flavor's measured window — probe run only, ` +
    `container boot, extension install, and VS Code startup excluded${compressedNote}</text>`;

  return (
    `<svg xmlns="http://www.w3.org/2000/svg" width="${WIDTH}" height="${height}" viewBox="0 0 ${WIDTH} ${height}">` +
    `<rect width="100%" height="100%" fill="#ffffff"/>` +
    `<text x="${MARGIN.left}" y="58" font-family="sans-serif" font-size="16" font-weight="700" fill="#111827">` +
    `VS Code resource usage — java-vsix-lite vs redhat.java (separate containers, same fixture)</text>` +
    legend +
    renderPanel({
      series,
      key: "cpu",
      yTop: cpuTop,
      height: CPU_HEIGHT,
      maxY: maxCpu,
      unit: "%",
      title: "Container CPU usage (% of one core)",
      toX,
    }) +
    renderPanel({
      series,
      key: "mem",
      yTop: memTop,
      height: MEM_HEIGHT,
      maxY: maxMem,
      unit: "MB",
      title: "Container memory usage",
      toX,
    }) +
    breakMarks +
    markerLines +
    markerLabels +
    xAxisLabel +
    `</svg>\n`
  );
}

function main() {
  const series = {};
  const durations = {};
  const events = [];

  for (const flavor of FLAVORS) {
    const raw = loadStats(flavor);
    const report = loadReport(flavor);
    if (raw.length === 0) {
      series[flavor] = [];
      continue;
    }
    // Clip to the measurement window the probe suite itself defines:
    // `flavorStartedAt` is the instant the suite began (VS Code already
    // booted, extension not yet activated) and `flavorEndedAt` the instant
    // it wrote its report. Samples outside that window are VS Code's own
    // startup and teardown — identical work for both plugins, and only
    // noise in a plugin-vs-plugin comparison. Falls back to the full
    // sample range if a report is missing those fields.
    const windowStart = report?.flavorStartedAt ?? raw[0].ts;
    const windowEnd = report?.flavorEndedAt ?? raw[raw.length - 1].ts;
    const clipped = raw.filter((p) => p.ts >= windowStart && p.ts <= windowEnd);
    const points = clipped.length >= 2 ? clipped : raw;
    const t0 = points === clipped ? windowStart : raw[0].ts;

    series[flavor] = points.map((p) => ({ elapsed: p.ts - t0, cpu: p.cpu, mem: p.mem }));
    durations[flavor] = {
      elapsed: points[points.length - 1].ts - t0,
      samples: points.length,
    };
    // The baseline's "events" are just its probes timing out with nothing
    // installed, so it contributes a trace for scale but no markers.
    if (flavor === "none") {
      continue;
    }
    for (const event of collectEvents(flavor, report)) {
      const elapsed = event.ts - t0;
      if (elapsed >= 0) {
        events.push({ ...event, elapsed });
      }
    }
  }

  events.sort((a, b) => a.elapsed - b.elapsed);
  const outFile = path.join(OUT_DIR, "timeline.svg");
  const total = FLAVORS.reduce((n, f) => n + (series[f] ?? []).length, 0);
  if (total < 2) {
    fs.writeFileSync(outFile, svgMessage("No resource samples were recorded (stats-*.csv missing or too short)."));
    console.log(`${outFile}: no data`);
    return;
  }

  fs.writeFileSync(outFile, renderSvg(series, events, durations));
  const counts = FLAVORS.map((f) => `${f}=${(series[f] ?? []).length}`).join(" ");
  console.log(`wrote ${outFile} (samples: ${counts}, ${events.length} markers)`);
}

main();
