import type { AgentSpec, AgentStatus, HubFrame, NodeFrame } from "@claudecord/protocol";
import type { Db, AgentRow } from "./db.js";

export interface NodeConn {
  nodeName: string;
  send(frame: HubFrame): void;
}

export interface Outbound {
  ensureProject(project: string): Promise<void>;
  post(project: string, agent: AgentRow, text: string, thread?: string): Promise<void>;
  postAsk(project: string, agent: AgentRow, ask: PendingAsk): Promise<void>;
  postReport(project: string, agent: AgentRow, title: string, summary: string, artifacts?: string[]): Promise<void>;
  notice(project: string, text: string, mention?: boolean): Promise<void>;
  refreshStatus(project: string): void;
}

export interface PendingAsk {
  askId: string;
  agentId: string;
  question: string;
  options?: string[];
  thread?: string;
}

const STREAK_LIMIT = 12;

/** Core state and routing. Discord-agnostic so it can be unit tested. */
export class Hub {
  nodes = new Map<string, NodeConn>();
  status = new Map<string, { status: AgentStatus; detail?: string }>();
  asks = new Map<string, PendingAsk[]>();
  private streak = new Map<string, number>();
  out!: Outbound;

  constructor(readonly db: Db) {}

  // Node lifecycle

  nodeConnected(conn: NodeConn): void {
    this.nodes.get(conn.nodeName)?.send({ t: "error", message: "replaced by new connection" });
    this.nodes.set(conn.nodeName, conn);
    this.db.clearAgentsOfNode(conn.nodeName);
  }

  nodeDisconnected(conn: NodeConn): void {
    if (this.nodes.get(conn.nodeName) !== conn) return;
    this.nodes.delete(conn.nodeName);
    const projects = new Set<string>();
    for (const a of this.db.allAgents().filter((a) => a.node_name === conn.nodeName)) {
      this.status.set(a.agent_id, { status: "offline" });
      projects.add(a.project);
    }
    for (const p of projects) this.out.refreshStatus(p);
  }

  // Frames from nodes

  async onNodeFrame(conn: NodeConn, f: NodeFrame): Promise<void> {
    switch (f.t) {
      case "hello":
        return;
      case "agent.register":
        return this.register(conn, f.agent);
      case "agent.status": {
        const a = this.agent(f.agentId);
        if (!a) return;
        this.status.set(f.agentId, { status: f.status, detail: f.detail });
        // The agent moved on (for example the prompt was answered in its terminal), so drop stale questions.
        if (f.status !== "waiting_input") this.dropAsks(a.project, f.agentId);
        this.out.refreshStatus(a.project);
        return;
      }
      case "agent.say": {
        const a = this.agent(f.agentId);
        if (!a) return;
        await this.out.post(a.project, a, f.text, f.thread);
        this.routeAgentMessage(a, f.text, f.thread);
        return;
      }
      case "agent.ask": {
        const a = this.agent(f.agentId);
        if (!a) return;
        const ask: PendingAsk = {
          askId: f.askId,
          agentId: f.agentId,
          question: f.question,
          options: f.options,
          thread: f.thread,
        };
        this.asks.set(a.project, [...(this.asks.get(a.project) ?? []), ask]);
        this.status.set(a.agent_id, { status: "waiting_input" });
        await this.out.postAsk(a.project, a, ask);
        this.out.refreshStatus(a.project);
        return;
      }
      case "agent.report": {
        const a = this.agent(f.agentId);
        if (!a) return;
        await this.out.postReport(a.project, a, f.title, f.summary, f.artifacts);
        return;
      }
      case "agent.limit": {
        const a = this.agent(f.agentId);
        if (!a) return;
        this.status.set(a.agent_id, { status: "limited", detail: f.kind });
        const when = f.resetsAt ? ` Resets ${f.resetsAt}.` : "";
        await this.out.notice(a.project, `${a.name} hit a ${f.kind} limit.${when}`, true);
        this.out.refreshStatus(a.project);
        return;
      }
      case "agent.gone": {
        const a = this.agent(f.agentId);
        if (!a) return;
        this.db.removeAgent(f.agentId);
        this.status.delete(f.agentId);
        this.dropAsks(a.project, f.agentId);
        this.out.refreshStatus(a.project);
        return;
      }
    }
  }

  private async register(conn: NodeConn, spec: AgentSpec): Promise<void> {
    await this.out.ensureProject(spec.project);
    const first = this.db.agentsOfProject(spec.project).length === 0;
    this.db.upsertAgent({
      agent_id: spec.agentId,
      name: spec.name,
      project: spec.project,
      node_name: conn.nodeName,
      adapter: spec.adapter,
      model: spec.model ?? null,
      role: spec.role ?? null,
      is_lead: first ? 1 : 0,
    });
    this.status.set(spec.agentId, { status: "idle" });
    const a = this.agent(spec.agentId)!;
    await this.out.notice(spec.project, `${a.name} joined (${a.adapter}${a.model ? `, ${a.model}` : ""}, ${a.node_name}).`);
    this.out.refreshStatus(spec.project);
    // Tell the newcomer who else is here.
    const peers = this.db.agentsOfProject(spec.project).filter((p) => p.agent_id !== spec.agentId);
    if (peers.length) {
      conn.send({
        t: "deliver",
        agentId: spec.agentId,
        from: "system",
        text: `Peers in this project: ${peers.map((p) => `${p.name}${p.is_lead ? " (lead)" : ""}`).join(", ")}.`,
      });
    }
  }

  // Routing

  agent(id: string): AgentRow | undefined {
    return this.db.allAgents().find((a) => a.agent_id === id);
  }

  findByName(project: string, name: string): AgentRow | undefined {
    return this.db.agentsOfProject(project).find((a) => a.name.toLowerCase() === name.toLowerCase());
  }

  private sendTo(a: AgentRow, frame: HubFrame): boolean {
    const n = this.nodes.get(a.node_name);
    if (!n) return false;
    n.send(frame);
    return true;
  }

  /** Message from the human in a project channel or thread. */
  humanMessage(project: string, text: string, thread?: string): string[] {
    this.streak.set(project, 0);
    const agents = this.db.agentsOfProject(project);
    const mentioned = agents.filter((a) => new RegExp(`(^|\\W)@${escapeRe(a.name)}(\\W|$)`, "i").test(text));

    // An open question gets answered first.
    const pending = this.asks.get(project) ?? [];
    const answerTargets = mentioned.length ? mentioned : agents.filter((a) => pending.some((p) => p.agentId === a.agent_id));
    if (answerTargets.length === 1 || (answerTargets.length && pending.length === 1)) {
      const target = answerTargets[0]!;
      const ask = pending.find((p) => p.agentId === target.agent_id && (!thread || p.thread === thread || !p.thread));
      if (ask) {
        this.dropAsk(project, ask.askId);
        this.status.set(target.agent_id, { status: "thinking" });
        this.sendTo(target, { t: "answer", agentId: target.agent_id, askId: ask.askId, text });
        this.out.refreshStatus(project);
        return [target.name];
      }
    }

    let targets = mentioned;
    if (!targets.length) {
      const lead = agents.find((a) => a.is_lead) ?? agents[0];
      targets = lead ? [lead] : [];
    }
    const clean = text.trim();
    for (const t of targets) {
      this.sendTo(t, { t: "deliver", agentId: t.agent_id, from: "engineer", text: clean, thread });
    }
    return targets.map((t) => t.name);
  }

  /** Agent chat is visible to peers. Mentioned peers get it, otherwise everyone. Loop guard applies. */
  private routeAgentMessage(from: AgentRow, text: string, thread?: string): void {
    const peers = this.db.agentsOfProject(from.project).filter((a) => a.agent_id !== from.agent_id);
    if (!peers.length) return;
    const mentionsHuman = /@engineer\b/i.test(text);
    const mentioned = peers.filter((a) => new RegExp(`(^|\\W)@${escapeRe(a.name)}(\\W|$)`, "i").test(text));
    const n = (this.streak.get(from.project) ?? 0) + 1;
    this.streak.set(from.project, n);
    if (n === STREAK_LIMIT) {
      void this.out.notice(from.project, "Agents have exchanged many messages without input. Pausing forwarding until you reply.", true);
    }
    if (n >= STREAK_LIMIT || mentionsHuman) return;
    const targets = mentioned.length ? mentioned : peers;
    for (const t of targets) {
      this.sendTo(t, { t: "deliver", agentId: t.agent_id, from: from.name, text, thread });
    }
  }

  private dropAsk(project: string, askId: string): void {
    this.asks.set(project, (this.asks.get(project) ?? []).filter((a) => a.askId !== askId));
  }

  private dropAsks(project: string, agentId: string): void {
    this.asks.set(project, (this.asks.get(project) ?? []).filter((a) => a.agentId !== agentId));
  }

  // Controls

  killall(project?: string): number {
    const targets = this.db.allAgents().filter((a) => !project || a.project === project);
    for (const n of new Set(targets.map((a) => a.node_name))) {
      this.nodes.get(n)?.send({ t: "killall", project });
    }
    return targets.length;
  }

  stop(project: string, name: string): boolean {
    const a = this.findByName(project, name);
    return !!a && this.sendTo(a, { t: "stop", agentId: a.agent_id });
  }

  hold(on: boolean, project?: string, name?: string): number {
    let targets = this.db.allAgents().filter((a) => !project || a.project === project);
    if (name && project) targets = targets.filter((a) => a.name.toLowerCase() === name.toLowerCase());
    for (const a of targets) {
      this.sendTo(a, { t: "hold", on, agentId: a.agent_id });
      this.status.set(a.agent_id, { status: on ? "paused" : "idle" });
    }
    for (const p of new Set(targets.map((a) => a.project))) this.out.refreshStatus(p);
    return targets.length;
  }

  spawn(nodeName: string, spec: AgentSpec): boolean {
    const n = this.nodes.get(nodeName);
    if (!n) return false;
    n.send({ t: "spawn", agent: spec });
    return true;
  }
}

function escapeRe(s: string): string {
  return s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}
