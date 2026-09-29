/** Token bucket. Allows short bursts and a sustained rate. */
export class Bucket {
  private tokens: number;
  private last: number;

  constructor(
    private capacity: number,
    private perSecond: number,
    now = Date.now(),
  ) {
    this.tokens = capacity;
    this.last = now;
  }

  take(n = 1, now = Date.now()): boolean {
    // Max with zero so a clock that steps backwards cannot drain the bucket.
    this.tokens = Math.min(this.capacity, this.tokens + (Math.max(0, now - this.last) / 1000) * this.perSecond);
    this.last = Math.max(this.last, now);
    if (this.tokens < n) return false;
    this.tokens -= n;
    return true;
  }
}

/** Counts failures per key inside a sliding window, so repeated bad tokens are refused without a database lookup. */
export class FailureLimiter {
  private hits = new Map<string, { n: number; resetAt: number }>();

  constructor(
    private max = 20,
    private windowMs = 60_000,
  ) {}

  blocked(key: string, now = Date.now()): boolean {
    const h = this.hits.get(key);
    if (!h) return false;
    if (now > h.resetAt) {
      this.hits.delete(key);
      return false;
    }
    return h.n >= this.max;
  }

  fail(key: string, now = Date.now()): void {
    const h = this.hits.get(key);
    if (!h || now > h.resetAt) this.hits.set(key, { n: 1, resetAt: now + this.windowMs });
    else h.n++;
    // Keep the map from growing without bound under a spray of addresses.
    if (this.hits.size > 10_000) for (const [k, v] of this.hits) if (now > v.resetAt) this.hits.delete(k);
  }
}
