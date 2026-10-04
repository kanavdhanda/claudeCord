import type { AgentRuntime } from "./agent.js";
import * as tmux from "./tmux.js";

/** How often the loop looks for agents that are due. Looking costs nothing, because it is not a process. */
const LOOP_MS = 250;

/**
 * One loop that watches every agent on this machine. Agents that are due for a look are sampled together with a
 * single batched tmux call, and each decides how soon it wants to be looked at again. A busy agent is watched closely
 * and an idle one hardly at all, so the cost of a machine full of idle agents is close to nothing.
 */
export class Sampler {
  private items = new Set<AgentRuntime>();
  private timer?: NodeJS.Timeout;
  private running = false;
  private stopped = true;

  add(rt: AgentRuntime): void {
    this.items.add(rt);
  }

  remove(rt: AgentRuntime): void {
    this.items.delete(rt);
  }

  start(): void {
    if (!this.stopped) return;
    this.stopped = false;
    this.schedule();
  }

  stop(): void {
    this.stopped = true;
    clearTimeout(this.timer);
  }

  private schedule(): void {
    if (this.stopped) return;
    this.timer = setTimeout(() => void this.round().finally(() => this.schedule()), LOOP_MS);
  }

  /** One pass: sample everything that is due. Exposed so a test can run a pass without waiting. */
  async round(now = Date.now()): Promise<void> {
    if (this.running) return;
    this.running = true;
    try {
      const due = [...this.items].filter((r) => !r.isStopped && r.dueAt <= now);
      if (!due.length) return;
      const screens = await tmux.sampleMany(due.map((r) => r.paneId));
      for (const r of due) {
        await r.sample(screens.get(r.paneId) ?? null);
        if (r.isStopped) this.items.delete(r);
      }
    } catch (e) {
      console.error("sampler round failed:", (e as Error).message);
    } finally {
      this.running = false;
    }
  }
}
