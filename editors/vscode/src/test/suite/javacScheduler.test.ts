import * as assert from "assert";
import { createJavacScheduler, AUTO_CHECK_MIN_INTERVAL_MS } from "../../javacScheduler";

function harness(
  opts: { enabled?: boolean; blocked?: boolean; outcome?: "completed" | "busy" } = {},
) {
  let now = 0;
  const timers: { at: number; cb: () => void; id: number }[] = [];
  let nextId = 1;
  const runs: { at: number; uris: string[] }[] = [];
  let enabled = opts.enabled ?? true;
  let blocked = opts.blocked ?? false;
  let outcome = opts.outcome ?? "completed";
  let resolveRun: (() => void) | undefined;
  const s = createJavacScheduler({
    now: () => now,
    setTimer: (cb, ms) => {
      const id = nextId++;
      timers.push({ at: now + ms, cb, id });
      return id;
    },
    clearTimer: (h) => {
      const i = timers.findIndex((t) => t.id === h);
      if (i >= 0) timers.splice(i, 1);
    },
    enabled: () => enabled,
    blocked: () => blocked,
    run: (uris) => {
      runs.push({ at: now, uris: [...uris] });
      return new Promise((res) => {
        resolveRun = () => res(outcome);
      });
    },
  });
  async function advance(ms: number) {
    const target = now + ms;
    for (;;) {
      timers.sort((a, b) => a.at - b.at);
      const t = timers[0];
      if (!t || t.at > target) break;
      now = t.at;
      timers.shift();
      t.cb();
      await Promise.resolve();
    }
    now = target;
    await Promise.resolve();
  }
  async function finishRun() {
    resolveRun?.();
    resolveRun = undefined;
    await Promise.resolve();
    await Promise.resolve();
  }
  return {
    s,
    runs,
    advance,
    finishRun,
    set: {
      enabled: (v: boolean) => (enabled = v),
      blocked: (v: boolean) => (blocked = v),
      outcome: (v: "completed" | "busy") => (outcome = v),
    },
  };
}

suite("javac scheduler", () => {
  test("first save dispatches after the 1.5s debounce; later saves wait for the 30s floor", async () => {
    const h = harness();
    h.s.enqueue("A");
    await h.advance(1_499);
    assert.strictEqual(h.runs.length, 0);
    await h.advance(1);
    assert.deepStrictEqual(h.runs, [{ at: 1_500, uris: ["A"] }]);
    await h.finishRun();

    h.s.enqueue("B");
    await h.advance(3_500);
    h.s.enqueue("C");
    h.s.enqueue("B");
    await h.advance(10_000);
    h.s.enqueue("D");
    await h.advance(AUTO_CHECK_MIN_INTERVAL_MS - 13_500 - 1);
    assert.strictEqual(h.runs.length, 1);
    await h.advance(1);
    assert.strictEqual(h.runs.length, 2);
    assert.deepStrictEqual([...h.runs[1].uris].sort(), ["B", "C", "D"]);
  });

  test("a save during a run stays queued and dispatches at the floor", async () => {
    const h = harness();
    h.s.enqueue("A");
    await h.advance(1_500);
    assert.strictEqual(h.runs.length, 1);
    h.s.enqueue("B");
    await h.finishRun();

    await h.advance(AUTO_CHECK_MIN_INTERVAL_MS - 1);
    assert.strictEqual(
      h.runs.length,
      1,
      "must wait for the 30s floor from the first run's start",
    );
    await h.advance(1);
    assert.strictEqual(h.runs.length, 2);
    assert.deepStrictEqual(h.runs[1].uris, ["B"]);
  });

  test("a run longer than 30s: next dispatch immediately after it ends", async () => {
    const h = harness();
    h.s.enqueue("A");
    await h.advance(1_500);
    assert.strictEqual(h.runs.length, 1);
    h.s.enqueue("B");
    await h.advance(40_000);
    assert.strictEqual(h.runs.length, 1, "still running; must not double-dispatch");
    await h.finishRun();
    assert.strictEqual(
      h.runs.length,
      2,
      "finishing a run past the floor must dispatch the queued batch immediately",
    );
    assert.deepStrictEqual(h.runs[1].uris, ["B"]);
  });

  test("busy requeues the same uris", async () => {
    const h = harness({ outcome: "busy" });
    h.s.enqueue("A");
    await h.advance(1_500);
    assert.strictEqual(h.runs.length, 1);
    await h.finishRun();
    h.set.outcome("completed");

    await h.advance(AUTO_CHECK_MIN_INTERVAL_MS);
    assert.strictEqual(h.runs.length, 2);
    assert.deepStrictEqual(h.runs[1].uris, ["A"]);
  });

  test("disabled at fire time drops the queue; cancelPending drops the timer", async () => {
    const h = harness({ enabled: false });
    h.s.enqueue("A");
    await h.advance(1_500);
    assert.strictEqual(h.runs.length, 0);

    h.set.enabled(true);
    h.s.enqueue("B");
    h.s.cancelPending();
    await h.advance(60_000);
    assert.strictEqual(h.runs.length, 0);
  });

  test("blocked (dirty buffer) keeps the batch until poke", async () => {
    const h = harness({ blocked: true });
    h.s.enqueue("A");
    await h.advance(1_500);
    assert.strictEqual(h.runs.length, 0);

    h.set.blocked(false);
    h.s.poke();
    await h.advance(1_500);
    assert.strictEqual(h.runs.length, 1);
    assert.deepStrictEqual(h.runs[0].uris, ["A"]);
  });

  test("dispose cancels", async () => {
    const h = harness();
    h.s.enqueue("A");
    h.s.dispose();
    await h.advance(60_000);
    assert.strictEqual(h.runs.length, 0);
  });

  test("reset detaches an in-flight run: its late completion neither re-queues nor blocks", async () => {
    const h = harness({ outcome: "busy" });
    h.s.enqueue("A");
    await h.advance(1_500);
    assert.strictEqual(h.runs.length, 1);
    h.s.reset();
    await h.finishRun(); // old run reports busy AFTER reset
    h.set.outcome("completed");
    h.s.enqueue("B");
    await h.advance(1_500);
    assert.strictEqual(
      h.runs.length,
      2,
      "a save after reset dispatches on the fresh debounce, not the old floor",
    );
    assert.deepStrictEqual(
      h.runs[1].uris,
      ["B"],
      "the detached run's busy result must not re-queue A",
    );
  });

  test("busy re-queue after reset is ignored but a later run still honours the floor", async () => {
    const h = harness();
    h.s.enqueue("A");
    await h.advance(1_500);
    await h.finishRun();
    h.s.enqueue("B");
    await h.advance(1_500);
    assert.strictEqual(h.runs.length, 1, "B waits for the 30s floor");
    h.s.reset();
    h.s.enqueue("C");
    await h.advance(1_500);
    assert.strictEqual(h.runs.length, 2, "reset clears the floor for the new client");
    assert.deepStrictEqual(h.runs[1].uris, ["C"]);
  });

  test("enqueue while blocked then unblocked within the debounce dispatches once", async () => {
    const h = harness({ blocked: true });
    h.s.enqueue("A");
    await h.advance(1_500);
    assert.strictEqual(h.runs.length, 0);
    h.set.blocked(false);
    h.s.poke();
    h.s.poke();
    await h.advance(0);
    assert.strictEqual(h.runs.length, 1, "double poke must not double-dispatch");
  });

  test("disabled during a run: busy result does not resurrect the queue", async () => {
    const h = harness({ outcome: "busy" });
    h.s.enqueue("A");
    await h.advance(1_500);
    h.set.enabled(false);
    await h.finishRun();
    await h.advance(60_000);
    assert.strictEqual(h.runs.length, 1);
  });
});
