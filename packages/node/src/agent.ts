import type { AgentSpec, AgentStatus } from "@claudecord/protocol";
import type { Adapter } from "./adapters/index.js";
import type { LimitInfo, PromptInfo } from "./adapters/types.js";
import { formatDeliveries, type Delivery } from "./rules.js";
import * as tmux from "./tmux.js";

export interface AgentEvents {
  status(agentId: string, status: AgentStatus, detail?: string): void;
  ask(agentId: string, askId: string, question: string, options: string[]): void;
  limit(agentId: string, info: LimitInfo): void;
  gone(agentId: string): void;
  /** The agent started on these deliveries. */
  accepted(agentId: string, msgIds: string[]): void;
}

/** How soon an agent that is doing something is looked at again. */
export const HOT_MS = 750;
/** How a quiet agent slows down: after 5 s of an unchanged screen, then after 30 s. */
export const WARM_MS = 1500;
export const COLD_MS = 3000;
export const WARM_AFTER_MS = 5_000;
export const COLD_AFTER_MS = 30_000;
const debug = (m: string) => process.env.CLAUDECORD_DEBUG && console.log(`[debug] ${m}`);
const INJECT_COOLDOWN_MS = 2500;

export class AgentRuntime {
  private queue: Delivery[] = [];
  private held = false;
  private limited = false;
  private sawReady = false;
  private rulesSent = false;
  private lastInject = 0;
  private pending?: { askId: string; info: PromptInfo };
  private blockingAsks = 0;
  private lastStatus?: AgentStatus;
  private sampling = false;
  private stopped = false;
  private lastScreen: string | null = null;
  private lastChange = Date.now();
  /** When the sampler should next look at this agent. Zero means as soon as possible. */
  dueAt = 0;
  private promptSeq = 0;
  private lastSig = "";
  private awaiting?: { ids: string[]; snippet: string };

  constructor(
    readonly spec: AgentSpec,
    readonly paneId: string,
    readonly cwd: string,
    private adapter: Adapter,
    private events: AgentEvents,
    private firstMessage?: string,
  ) {}

  get isStopped(): boolean {
    return this.stopped;
  }

  /** Look at this agent on the next pass, because something just happened that it should react to. */
  wake(): void {
    this.dueAt = 0;
  }

  async stop(): Promise<void> {
    this.stopped = true;
    await tmux.killPane(this.paneId);
  }

  enqueue(d: Delivery): void {
    this.queue.push(d);
    this.wake();
  }

  /** The agent spoke or asked something, which proves it is working on what it was given. */
  noteActivity(): void {
    this.markAccepted();
  }

  private markAccepted(): void {
    if (!this.awaiting) return;
    const ids = this.awaiting.ids;
    this.awaiting = undefined;
    if (ids.length) this.events.accepted(this.spec.agentId, ids);
  }

  setHeld(on: boolean): void {
    this.held = on;
    this.wake();
  }

  /** Blocking MCP or shell asks are tracked so status shows waiting and injection pauses. */
  beginAsk(): void {
    this.blockingAsks++;
    this.wake();
  }

  endAsk(): void {
    this.blockingAsks = Math.max(0, this.blockingAsks - 1);
    this.wake();
  }

  hasPrompt(askId: string): boolean {
    return this.pending?.askId === askId;
  }

  /** Maps a free-text human answer onto the menu that is on screen. Returns false if it cannot. */
  async answerPrompt(askId: string, text: string): Promise<boolean> {
    const p = this.pending;
    if (!p || p.askId !== askId) return false;
    this.wake();
    const t = text.trim();
    const n = /^\d{1,2}$/.test(t) ? Number(t) - 1 : -1;
    let idx = n >= 0 && n < p.info.options.length ? n : -1;
    if (idx < 0) {
      const low = t.toLowerCase();
      idx = p.info.options.findIndex((o) => o.toLowerCase() === low);
      if (idx < 0) idx = p.info.options.findIndex((o) => o.toLowerCase().startsWith(low));
    }
    if (idx >= 0) {
      this.pending = undefined;
      await tmux.sendKeys(this.paneId, this.adapter.selectKeys(p.info, idx));
      return true;
    }
    const other = this.adapter.otherKeys(p.info);
    if (other) {
      this.pending = undefined;
      await tmux.sendKeys(this.paneId, other);
      await sleep(500);
      await tmux.pasteAndSubmit(this.paneId, t);
      return true;
    }
    if (/^(y|yes|approve|ok|go ahead)\b/i.test(t)) {
      this.pending = undefined;
      await tmux.sendKeys(this.paneId, this.adapter.selectKeys(p.info, 0));
      return true;
    }
    return false;
  }

  private emitStatus(s: AgentStatus, detail?: string): void {
    if (s === this.lastStatus) return;
    this.lastStatus = s;
    this.events.status(this.spec.agentId, s, detail);
  }

  /**
   * Reacts to what the pane shows right now. `screen` is null when the pane no longer exists. Called by the sampler,
   * which captures many panes at once, so this does no capturing of its own.
   */
  async sample(screen: string | null): Promise<void> {
    if (this.sampling || this.stopped) return;
    this.sampling = true;
    try {
      if (screen === null) {
        this.stopped = true;
        this.events.gone(this.spec.agentId);
        return;
      }
      const now = Date.now();
      if (screen !== this.lastScreen) {
        this.lastScreen = screen;
        this.lastChange = now;
      }
      const st = this.adapter.detect(screen);
      const sig = `${st.ready}/${st.busy}/${!!st.prompt}/${!!st.limit}/q${this.queue.length}/h${this.held}/a${this.blockingAsks}`;
      if (sig !== this.lastSig) {
        this.lastSig = sig;
        debug(`${this.spec.name} ready/busy/prompt/limit/queue/held/asks = ${sig}`);
      }

      if (st.prompt) {
        if (!this.sawReady) {
          const choice = this.adapter.startupChoice(st.prompt);
          if (choice !== undefined && choice >= 0) {
            await tmux.sendKeys(this.paneId, this.adapter.selectKeys(st.prompt, choice));
            await sleep(800);
            return;
          }
        }
        if (this.pending?.info.signature !== st.prompt.signature) {
          const askId = `${this.spec.agentId}#p${++this.promptSeq}`;
          this.pending = { askId, info: st.prompt };
          const lines = st.prompt.options.map((o, i) => `${i + 1}. ${o}`).join("\n");
          this.events.ask(this.spec.agentId, askId, `${st.prompt.question}\n${lines}`, st.prompt.options);
        }
        this.emitStatus("waiting_input");
        return;
      }
      this.pending = undefined;

      if (
        this.awaiting &&
        (st.busy || (this.awaiting.snippet && screen.replace(/\s+/g, " ").includes(this.awaiting.snippet)))
      ) {
        this.markAccepted();
      }

      if (st.limit && !this.limited) {
        this.limited = true;
        this.events.limit(this.spec.agentId, st.limit);
      } else if (!st.limit && this.limited) {
        this.limited = false;
      }

      if (st.ready) this.sawReady = true;
      if (st.ready && !this.rulesSent && this.firstMessage) {
        this.rulesSent = true;
        await tmux.pasteAndSubmit(this.paneId, this.firstMessage);
        this.lastInject = Date.now();
        return;
      }

      let s: AgentStatus;
      if (this.blockingAsks > 0) s = "waiting_input";
      else if (this.limited) s = "limited";
      else if (!this.sawReady) s = "starting";
      else if (this.held) s = "paused";
      else if (st.busy) s = st.executing ? "executing" : "thinking";
      else s = "idle";
      this.emitStatus(s, this.limited ? st.limit?.kind : undefined);

      const canDeliver =
        st.ready &&
        this.sawReady &&
        !this.held &&
        !this.limited &&
        this.blockingAsks === 0 &&
        Date.now() - this.lastInject > INJECT_COOLDOWN_MS;
      if (canDeliver && this.queue.length) {
        const batch = this.queue.splice(0);
        this.lastInject = Date.now();
        // Anything still awaiting acceptance from an earlier batch is superseded by this one.
        const ids = batch.map((b) => b.msgId).filter((x): x is string => !!x);
        const last = batch[batch.length - 1]!;
        this.awaiting = {
          ids: [...(this.awaiting?.ids ?? []), ...ids],
          snippet: last.text.replace(/\s+/g, " ").slice(0, 24),
        };
        debug(`${this.spec.name} injecting ${batch.length} message(s)`);
        await tmux.pasteAndSubmit(this.paneId, formatDeliveries(batch));
        this.emitStatus("thinking");
      }
    } catch (e) {
      console.error(`[${this.spec.name}] sample failed:`, (e as Error).message);
    } finally {
      this.sampling = false;
      this.dueAt = alignedDue(Date.now(), this.nextInterval());
    }
  }

  /**
   * An agent that is working, starting up, or waiting for something is watched closely. One that has shown the same
   * screen for a while is watched less and less, and anything that matters wakes it at once.
   */
  nextInterval(now = Date.now()): number {
    const needsAttention =
      !this.sawReady ||
      this.queue.length > 0 ||
      !!this.awaiting ||
      !!this.pending ||
      this.blockingAsks > 0 ||
      this.lastStatus === "thinking" ||
      this.lastStatus === "executing";
    if (needsAttention) return HOT_MS;
    const quiet = now - this.lastChange;
    if (quiet >= COLD_AFTER_MS) return COLD_MS;
    if (quiet >= WARM_AFTER_MS) return WARM_MS;
    return HOT_MS;
  }
}

/**
 * The next time to look, snapped to a grid of the interval's own size. Every agent with the same pace lands on the
 * same instant, so a machine full of idle agents is checked in one batch instead of one agent at a time. Without this
 * they drift apart and almost every pass has a single agent due, which defeats the batching. The intervals are all
 * multiples of 750 ms so the grids line up.
 */
export function alignedDue(now: number, interval: number): number {
  return Math.ceil((now + interval / 2) / interval) * interval;
}

function sleep(ms: number): Promise<void> {
  return new Promise((r) => setTimeout(r, ms));
}
