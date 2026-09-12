// High-frequency container resource sampler: reads this container's own
// cgroup v2 counters and appends `epochMs,cpuPercent,memMB` rows to argv[2].
//
// Node rather than a shell loop because the shell version forked four
// processes per sample (`date`, two `awk`s, `cat`); at any useful rate that
// overhead lands in the very cgroup counters being measured. One long-lived
// process doing two `readFileSync`s per tick costs almost nothing, which
// matters because java-vsix-lite's whole measured window can be a few
// hundred milliseconds — at the old 5 Hz that was two samples.
//
// Started by runFlavor.js only after the VSIX is installed and the settle
// period has elapsed, so container creation, Xvfb boot, and the extension
// install all stay outside the measured window.
const fs = require("node:fs");

const OUT_FILE = process.argv[2];
if (!OUT_FILE) {
  console.error("usage: sampleStats.js <output-csv>");
  process.exit(2);
}
const HZ = Math.max(1, Number(process.env.JVL_COMPARE_SAMPLE_HZ ?? 20));
const INTERVAL_MS = 1000 / HZ;

function readCpuUsec() {
  try {
    const stat = fs.readFileSync("/sys/fs/cgroup/cpu.stat", "utf8");
    const match = stat.match(/^usage_usec (\d+)/m);
    return match ? Number(match[1]) : null;
  } catch {
    return null;
  }
}

function readMemBytes() {
  try {
    return Number(fs.readFileSync("/sys/fs/cgroup/memory.current", "utf8").trim());
  } catch {
    return null;
  }
}

fs.writeFileSync(OUT_FILE, "");
const stream = fs.createWriteStream(OUT_FILE, { flags: "a" });

let prevUsec = readCpuUsec();
let prevNs = process.hrtime.bigint();

setInterval(() => {
  const usec = readCpuUsec();
  const mem = readMemBytes();
  const nowNs = process.hrtime.bigint();
  if (usec === null || mem === null || prevUsec === null) {
    prevUsec = usec;
    prevNs = nowNs;
    return;
  }
  const deltaUsec = usec - prevUsec;
  const deltaNs = Number(nowNs - prevNs);
  prevUsec = usec;
  prevNs = nowNs;
  if (deltaNs <= 0) {
    return;
  }
  // CPU time consumed / wall time elapsed, as a percentage of one core.
  const cpuPercent = (deltaUsec * 1000 * 100) / deltaNs;
  stream.write(`${Date.now()},${cpuPercent.toFixed(2)},${(mem / 1048576).toFixed(2)}\n`);
  // The interval itself keeps this process alive until runFlavor.js kills
  // it once the probe suite has finished.
}, INTERVAL_MS);
