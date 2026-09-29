/**
 * Counters for the dashboard's insights. Recording an event is one array write, so it costs nothing on the message
 * path. Values live in a ring of one-minute buckets covering a day, and are saved sparsely, so a restart keeps history
 * and the saved form stays tiny.
 */

const MINUTE = 60_000;

/** Upper bounds, in ms, of the acceptance latency buckets. Anything slower lands in the last one. */
export const ACCEPT_BOUNDS = [1000, 5000, 15_000, 60_000, Infinity] as const;
const ACCEPT_KEYS = ["accept_1", "accept_5", "accept_15", "accept_60", "accept_slow"] as const;

class Ring {
  readonly vals: Float64Array;
  readonly stamp: Int32Array;
  constructor(readonly size: number) {
    this.vals = new Float64Array(size);
    this.stamp = new Int32Array(size).fill(-1);
  }
  add(minute: number, n: number): void {
    const i = minute % this.size;
    if (this.stamp[i] !== minute) {
      this.stamp[i] = minute;
      this.vals[i] = 0;
    }
    this.vals[i]! += n;
  }
  get(minute: number): number {
    const i = minute % this.size;
    return this.stamp[i] === minute ? this.vals[i]! : 0;
  }
}

export interface Insights {
  rangeMinutes: number;
  bucketMinutes: number;
  start: number;
  series: Record<string, number[]>;
  totals: Record<string, number>;
  acceptance: { n: number; p50Ms: number | null; p95Ms: number | null; buckets: number[] };
  taskCycle: { n: number; avgMs: number | null };
}

export class Metrics {
  private rings = new Map<string, Ring>();
  private busy = new Map<string, { ms: number; since?: number }>();

  constructor(
    private now: () => number = Date.now,
    readonly minutes = 1440,
  ) {}

  private ring(key: string): Ring {
    let r = this.rings.get(key);
    if (!r) this.rings.set(key, (r = new Ring(this.minutes)));
    return r;
  }

  inc(key: string, n = 1): void {
    this.ring(key).add(Math.floor(this.now() / MINUTE), n);
  }

  /** Records how long an agent took to pick up a message. */
  accepted(ms: number): void {
    const i = ACCEPT_BOUNDS.findIndex((b) => ms < b);
    this.inc(ACCEPT_KEYS[i === -1 ? ACCEPT_KEYS.length - 1 : i]!);
    this.inc("accept_sum_ms", ms);
  }

  taskFinished(cycleMs: number): void {
    this.inc("task_done");
    this.inc("task_cycle_ms", cycleMs);
  }

  /** Tracks how long each agent spends working, from its status changes. */
  status(agentId: string, status: string): void {
    const t = this.now();
    const b = this.busy.get(agentId) ?? { ms: 0 };
    const working = status === "thinking" || status === "executing";
    if (working && b.since === undefined) b.since = t;
    else if (!working && b.since !== undefined) {
      b.ms += t - b.since;
      b.since = undefined;
    }
    this.busy.set(agentId, b);
  }

  forget(agentId: string): void {
    this.busy.delete(agentId);
  }

  busiest(limit = 5): { agentId: string; busyMs: number }[] {
    const t = this.now();
    return [...this.busy.entries()]
      .map(([agentId, b]) => ({ agentId, busyMs: b.ms + (b.since !== undefined ? t - b.since : 0) }))
      .filter((x) => x.busyMs > 0)
      .sort((a, b) => b.busyMs - a.busyMs)
      .slice(0, limit);
  }

  private sum(key: string, from: number, to: number): number {
    const r = this.rings.get(key);
    if (!r) return 0;
    let s = 0;
    for (let m = from; m <= to; m++) s += r.get(m);
    return s;
  }

  /** A chart-ready view of the last `rangeMinutes`, in buckets of `bucketMinutes`. */
  insights(rangeMinutes: number, bucketMinutes: number, seriesKeys: string[]): Insights {
    const range = Math.min(rangeMinutes, this.minutes);
    const end = Math.floor(this.now() / MINUTE);
    const start = end - range + 1;
    const n = Math.ceil(range / bucketMinutes);
    const series: Record<string, number[]> = {};
    for (const k of seriesKeys) {
      const out = Array<number>(n).fill(0);
      const r = this.rings.get(k);
      if (r)
        for (let m = start; m <= end; m++) out[Math.min(n - 1, Math.floor((m - start) / bucketMinutes))]! += r.get(m);
      series[k] = out;
    }
    const totals: Record<string, number> = {};
    for (const k of this.rings.keys()) totals[k] = this.sum(k, start, end);

    const buckets = ACCEPT_KEYS.map((k) => totals[k] ?? 0);
    const count = buckets.reduce((a, b) => a + b, 0);
    const pct = (q: number): number | null => {
      if (!count) return null;
      let cum = 0;
      for (let i = 0; i < buckets.length; i++) {
        cum += buckets[i]!;
        if (cum / count >= q) return ACCEPT_BOUNDS[i] === Infinity ? ACCEPT_BOUNDS[3]! : ACCEPT_BOUNDS[i]!;
      }
      return null;
    };
    const cycles = totals.task_done ?? 0;
    return {
      rangeMinutes: range,
      bucketMinutes,
      start: start * MINUTE,
      series,
      totals,
      acceptance: { n: count, p50Ms: pct(0.5), p95Ms: pct(0.95), buckets },
      taskCycle: { n: cycles, avgMs: cycles ? Math.round((totals.task_cycle_ms ?? 0) / cycles) : null },
    };
  }

  /** Only the non-zero buckets, so the saved form is small however long the hub has run. */
  toJSON(): Record<string, Record<number, number>> {
    const out: Record<string, Record<number, number>> = {};
    const end = Math.floor(this.now() / MINUTE);
    for (const [k, r] of this.rings) {
      const m: Record<number, number> = {};
      for (let t = end - this.minutes + 1; t <= end; t++) {
        const v = r.get(t);
        if (v) m[t] = v;
      }
      if (Object.keys(m).length) out[k] = m;
    }
    return out;
  }

  load(data: unknown): void {
    if (!data || typeof data !== "object") return;
    const end = Math.floor(this.now() / MINUTE);
    for (const [k, m] of Object.entries(data as Record<string, unknown>)) {
      if (!m || typeof m !== "object") continue;
      for (const [t, v] of Object.entries(m as Record<string, unknown>)) {
        const minute = Number(t);
        if (
          Number.isInteger(minute) &&
          typeof v === "number" &&
          Number.isFinite(v) &&
          minute > end - this.minutes &&
          minute <= end
        ) {
          this.ring(k).add(minute, v);
        }
      }
    }
  }
}
