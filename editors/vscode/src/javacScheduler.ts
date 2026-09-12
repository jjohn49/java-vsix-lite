// Automatic javac backstop scheduler. Pure (no vscode import) so it is unit-testable with a virtual clock.
export const AUTO_CHECK_DEBOUNCE_MS = 1_500;
export const AUTO_CHECK_MIN_INTERVAL_MS = 30_000;

export type RunOutcome = "completed" | "busy";

export interface SchedulerDeps {
  now(): number; // monotonic ms (performance.now)
  setTimer(cb: () => void, ms: number): unknown;
  clearTimer(handle: unknown): void;
  enabled(): boolean; // full gate: trusted + folder + setting + client alive
  blocked(): boolean; // any dirty file-backed Java buffer -> javac would compile stale disk
  run(uris: readonly string[]): Promise<RunOutcome>;
}

export interface JavacScheduler {
  enqueue(uri: string): void; // a Java file was saved
  poke(): void; // re-evaluate (a dirty buffer closed/saved; setting re-enabled)
  cancelPending(): void; // setting disabled / client restarting: drop timer + queue
  reset(): void; // client restarted: drop timer + queue AND forget an in-flight run
  dispose(): void;
}

export function createJavacScheduler(deps: SchedulerDeps): JavacScheduler {
  const pending = new Set<string>();
  let timer: unknown = undefined;
  let running = false;
  let runToken = 0;
  let lastStart = Number.NEGATIVE_INFINITY;
  // Anchor for the debounce window: when the current batch started
  // accumulating. +Infinity when `pending` is empty (no deadline yet); a
  // busy requeue sets it to -Infinity so only the 30s floor governs the
  // retry, not a fresh debounce.
  let pendingSince = Number.POSITIVE_INFINITY;
  let disposed = false;

  function arm(): void {
    if (disposed || timer !== undefined || running || pending.size === 0) return;
    const now = deps.now();
    const deadline = Math.max(
      pendingSince + AUTO_CHECK_DEBOUNCE_MS,
      lastStart + AUTO_CHECK_MIN_INTERVAL_MS,
    );
    if (deadline <= now) {
      // Debounce window and 30s floor already elapsed; dispatch now
      // instead of waiting further.
      fire();
      return;
    }
    timer = deps.setTimer(fire, deadline - now);
  }
  function fire(): void {
    timer = undefined;
    if (disposed) return;
    if (!deps.enabled()) {
      pending.clear();
      pendingSince = Number.POSITIVE_INFINITY;
      return;
    }
    if (deps.blocked()) return; // stay queued; poke() re-arms
    if (pending.size === 0) return;
    const uris = [...pending];
    pending.clear();
    pendingSince = Number.POSITIVE_INFINITY;
    running = true;
    lastStart = deps.now(); // start-to-start spacing, regardless of outcome
    const token = ++runToken;
    void deps
      .run(uris)
      .then(
        (outcome) => {
          if (token !== runToken) return; // reset() happened; this run is history
          if (outcome === "busy") {
            for (const u of uris) pending.add(u);
            pendingSince = Number.NEGATIVE_INFINITY;
          }
        },
        () => {
          /* failed: wait for the next save */
        },
      )
      .finally(() => {
        if (token !== runToken) return;
        running = false;
        arm();
      });
  }
  function cancelPendingImpl(): void {
    if (timer !== undefined) {
      deps.clearTimer(timer);
      timer = undefined;
    }
    pending.clear();
    pendingSince = Number.POSITIVE_INFINITY;
  }
  return {
    enqueue(uri) {
      if (disposed) return;
      if (pending.size === 0) pendingSince = deps.now();
      pending.add(uri);
      arm();
    },
    poke() {
      arm();
    },
    cancelPending() {
      cancelPendingImpl();
    },
    reset() {
      cancelPendingImpl();
      runToken += 1; // detach any in-flight run: its settle callbacks become no-ops
      running = false;
      lastStart = Number.NEGATIVE_INFINITY; // a fresh client gets a fresh floor
    },
    dispose() {
      disposed = true;
      if (timer !== undefined) {
        deps.clearTimer(timer);
        timer = undefined;
      }
      pending.clear();
      pendingSince = Number.POSITIVE_INFINITY;
    },
  };
}
