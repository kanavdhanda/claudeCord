import { FILE_CHUNK_BYTES, MAX_FILE_BYTES, type AgentSpec, type AgentStatus, type HubFrame, type NodeFrame } from "@claudecord/protocol";
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
  postFile(project: string, agent: AgentRow, name: string, data: Buffer, caption?: string, thread?: string): Promise<void>;
  notice(project: string, text: string, mention?: boolean): Promise<void>;
  /** Marks a human message as accepted by an agent, for example with a reaction. */
  confirm(project: string, ref: string, agentName: string): Promise<void>;
  refreshStatus(project: string): void;
}

export interface RouteResult {
  /** Agents the message was sent to. */
  targets: string[];
  /** Targets whose node is not connected, so nothing was delivered. */
  offline: string[];
  /** Targets that will not pick the message up right away, with the reason. */
  held: { name: string; why: string }[];
}

interface Pending {
  project: string;
  agentId: string;
  /** Opaque Discord message reference of the human message, if any. */
  ref?: string;
  taskId?: string;
}

export interface PendingAsk {
  askId: string;
  agentId: string;
  question: string;
  options?: string[];
  thread?: string;
}

const DEFAULT_STREAK_LIMIT = 20;
const DEFAULT_ACCEPT_TIMEOUT_MS = 30_000;
const HOLD_REASONS: Partial<Record<AgentStatus, string>> = {
  paused: "paused",
  limited: "at a usage limit",
  waiting_input: "waiting on an answer",
  offline: "offline",
  starting: "still starting",
};

/** Core state and routing. Discord-agnostic so it can be unit tested. */
export class Hub {
  nodes = new Map<string, NodeConn>();
  status = new Map<string, { status: AgentStatus; detail?: string }>();
  asks = new Map<string, PendingAsk[]>();
  private streak = new Map<string, number>();
  private uploads = new Map<string, { chunks: Buffer[]; bytes: number }>();
  private pending = new Map<string, Pending>();
  private seq = 0;
  out!: Outbound;

  constructor(
    readonly db: Db,
    private streakLimit = DEFAULT_STREAK_LIMIT,
    private acceptTimeoutMs = DEFAULT_ACCEPT_TIMEOUT_MS,
  ) {}

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
    for (const a of this.db.agentsOfNode(conn.nodeName)) {
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
      case "agent.accepted":
        return this.onAccepted(f.agentId, f.msgIds);
      case "agent.assign":
        return this.assign(f.agentId, f.to, f.task, f.thread);
      case "agent.taskdone":
        return this.taskDone(f.agentId, f.taskId, f.summary);
      case "file.chunk":
        return this.onFileChunk(f);
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
    await this.briefOnJoin(a);
  }

  /** Tells the newcomer who is here and what its role is, and lets the lead know about new peers. */
  private async briefOnJoin(a: AgentRow): Promise<void> {
    const peers = this.db.agentsOfProject(a.project).filter((p) => p.agent_id !== a.agent_id);
    const lead = this.db.agentsOfProject(a.project).find((p) => p.is_lead);
    if (a.is_lead) {
      this.system(a, this.leadBrief(a, peers));
    } else if (lead) {
      this.system(a, this.workerBrief(a, lead, peers));
      this.system(lead, `${a.name} joined (${a.adapter}${a.model ? `, ${a.model}` : ""}${a.role ? `, ${a.role}` : ""}). You can assign work to them.`);
    }
  }

  private leadBrief(a: AgentRow, peers: AgentRow[]): string {
    if (!peers.length) {
      return `You are the lead of project ${a.project} and nobody else is here yet. Do the work yourself. If peers join you will be told, and then you can split work.`;
    }
    return [
      `You are the lead of project ${a.project}. Peers: ${peers.map((p) => `${p.name}${p.role ? ` (${p.role})` : ""}`).join(", ")}.`,
      "When the engineer gives a task: discuss the approach briefly in chat, split it into subtasks, post the plan in a thread, and give each subtask to one peer with the assign tool (agent, task). Do not do your peers' work.",
      "Each assignment is tracked as a task id. You are told when a peer accepts and when they finish. When every task is done, integrate the results and send the single final report.",
    ].join("\n");
  }

  private workerBrief(a: AgentRow, lead: AgentRow, peers: AgentRow[]): string {
    return [
      `${lead.name} is the lead of project ${a.project}. Peers: ${[lead, ...peers.filter((p) => p.agent_id !== lead.agent_id)].map((p) => p.name).join(", ")}.`,
      "Take assignments from the lead. Each arrives as a task with an id. Work on it, then call task_done with that id and a short summary. Ask the lead for clarification before asking the engineer.",
    ].join("\n");
  }

  private system(a: AgentRow, text: string): void {
    this.sendTo(a, { t: "deliver", agentId: a.agent_id, from: "system", text });
  }

  setLead(project: string, agentId: string): AgentRow | undefined {
    const a = this.agent(agentId);
    if (!a || a.project !== project) return undefined;
    this.db.setLead(project, agentId);
    const all = this.db.agentsOfProject(project);
    for (const m of all) {
      const peers = all.filter((p) => p.agent_id !== m.agent_id);
      this.system(m, m.agent_id === agentId ? this.leadBrief(m, peers) : this.workerBrief(m, a, peers));
    }
    this.out.refreshStatus(project);
    return a;
  }

  // Acceptance and tasks

  /** Sends a delivery and, when something should be confirmed, tracks it until the node reports acceptance. */
  private deliver(a: AgentRow, from: string, text: string, o: { thread?: string; ref?: string; taskId?: string } = {}): boolean {
    let msgId: string | undefined;
    if (o.ref || o.taskId) {
      msgId = `m${++this.seq}`;
      this.pending.set(msgId, { project: a.project, agentId: a.agent_id, ref: o.ref, taskId: o.taskId });
    }
    const sent = this.sendTo(a, { t: "deliver", agentId: a.agent_id, from, text, thread: o.thread, msgId });
    if (!sent && msgId) this.pending.delete(msgId);
    return sent;
  }

  private async onAccepted(agentId: string, msgIds: string[]): Promise<void> {
    const a = this.agent(agentId);
    if (!a) return;
    for (const id of msgIds) {
      const p = this.pending.get(id);
      if (!p || p.agentId !== agentId) continue;
      this.pending.delete(id);
      if (p.ref) await this.out.confirm(p.project, p.ref, a.name).catch(() => {});
      if (p.taskId) {
        this.db.setTaskState(p.project, p.taskId, "accepted");
        await this.out.notice(p.project, `${a.name} accepted ${p.taskId}.`);
      }
    }
  }

  private async assign(fromId: string, toName: string, task: string, thread?: string): Promise<void> {
    const from = this.agent(fromId);
    if (!from) return;
    if (!from.is_lead) {
      const lead = this.db.agentsOfProject(from.project).find((p) => p.is_lead);
      this.system(from, `Only the lead assigns tasks.${lead ? ` Ask ${lead.name} if you need something done.` : ""}`);
      return;
    }
    const to = this.findByName(from.project, toName);
    if (!to || to.agent_id === from.agent_id) {
      this.system(from, `Cannot assign to ${toName}. Peers: ${this.db.agentsOfProject(from.project).filter((p) => p.agent_id !== from.agent_id).map((p) => p.name).join(", ") || "none"}.`);
      return;
    }
    const t = this.db.createTask(from.project, from.agent_id, to.agent_id, task);
    const sent = this.deliver(to, from.name, `Task ${t.id}: ${task}\nWhen finished, call task_done with id ${t.id} and a short summary.`, { thread, taskId: t.id });
    await this.out.post(from.project, from, `@${to.name} ${t.id}: ${task}`, thread);
    if (!sent) this.system(from, `${to.name} is offline, so ${t.id} was not delivered.`);
  }

  private async taskDone(agentId: string, taskId: string, summary: string): Promise<void> {
    const a = this.agent(agentId);
    if (!a) return;
    const t = this.db.getTask(a.project, taskId);
    if (!t || t.to_agent !== a.agent_id) {
      this.system(a, `Task ${taskId} is not assigned to you.`);
      return;
    }
    this.db.setTaskState(a.project, t.id, "done", summary);
    await this.out.post(a.project, a, `Finished ${t.id}: ${summary}`);
    const lead = this.agent(t.from_agent);
    if (lead) {
      this.system(lead, `${a.name} finished ${t.id}: ${summary}`);
      const all = this.db.tasksOfProject(a.project);
      if (all.length && all.every((x) => x.state === "done")) {
        this.system(lead, `All ${all.length} task(s) are done. Integrate the results and send the single final report.`);
        await this.out.notice(a.project, `All ${all.length} task(s) are done.`);
      }
    }
  }

  // Files

  /** Peer transfers are relayed chunk by chunk. Transfers to Discord are buffered and posted once. */
  private async onFileChunk(f: Extract<NodeFrame, { t: "file.chunk" }>): Promise<void> {
    const a = this.agent(f.agentId);
    if (!a) return;
    if (f.to) {
      const peer = this.findByName(a.project, f.to);
      if (!peer) {
        if (f.seq === 0) await this.out.notice(a.project, `${a.name} tried to send a file to ${f.to}, but no such agent is in this project.`);
        return;
      }
      this.sendTo(peer, {
        t: "file.chunk", transferId: f.transferId, agentId: peer.agent_id, from: a.name,
        name: f.name, seq: f.seq, last: f.last, data: f.data, caption: f.caption, thread: f.thread,
      });
      if (f.last) await this.out.post(a.project, a, `Sent ${f.name} to @${peer.name}.${f.caption ? ` ${f.caption}` : ""}`, f.thread);
      return;
    }
    let u = this.uploads.get(f.transferId);
    if (!u) {
      if (f.seq !== 0) return;
      u = { chunks: [], bytes: 0 };
      this.uploads.set(f.transferId, u);
    }
    const buf = Buffer.from(f.data, "base64");
    u.bytes += buf.length;
    if (u.bytes > MAX_FILE_BYTES) {
      this.uploads.delete(f.transferId);
      await this.out.notice(a.project, `${a.name} tried to send ${f.name}, which is over the ${MAX_FILE_BYTES / 1048576} MB limit.`);
      return;
    }
    u.chunks.push(buf);
    if (!f.last) return;
    this.uploads.delete(f.transferId);
    await this.out.postFile(a.project, a, f.name, Buffer.concat(u.chunks), f.caption, f.thread);
  }

  /** Sends a file from the human to the agents the message addresses (mentions, otherwise the lead). */
  sendFile(project: string, text: string, name: string, data: Buffer, thread?: string): string[] {
    const targets = this.pickTargets(project, text);
    const transferId = `h-${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 8)}`;
    const total = Math.max(1, Math.ceil(data.length / FILE_CHUNK_BYTES));
    for (const t of targets) {
      for (let seq = 0; seq < total; seq++) {
        this.sendTo(t, {
          t: "file.chunk", transferId, agentId: t.agent_id, from: "engineer", name, seq, last: seq === total - 1,
          data: data.subarray(seq * FILE_CHUNK_BYTES, (seq + 1) * FILE_CHUNK_BYTES).toString("base64"),
          caption: text || undefined, thread,
        });
      }
    }
    return targets.map((t) => t.name);
  }

  // Routing

  private pickTargets(project: string, text: string): AgentRow[] {
    const agents = this.db.agentsOfProject(project);
    const mentioned = agents.filter((a) => new RegExp(`(^|\\W)@${escapeRe(a.name)}(\\W|$)`, "i").test(text));
    if (mentioned.length) return mentioned;
    const lead = agents.find((a) => a.is_lead) ?? agents[0];
    return lead ? [lead] : [];
  }

  agent(id: string): AgentRow | undefined {
    return this.db.getAgent(id);
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

  /** Message from the human in a project channel or thread. `ref` identifies it for the acceptance confirmation. */
  humanMessage(project: string, text: string, thread?: string, ref?: string): RouteResult {
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
        const ok = this.sendTo(target, { t: "answer", agentId: target.agent_id, askId: ask.askId, text });
        this.out.refreshStatus(project);
        return ok ? { targets: [target.name], offline: [], held: [] } : { targets: [target.name], offline: [target.name], held: [] };
      }
    }

    const targets = mentioned.length ? mentioned : this.pickTargets(project, text);
    const res: RouteResult = { targets: targets.map((t) => t.name), offline: [], held: [] };
    const clean = text.trim();
    for (const t of targets) {
      if (!this.deliver(t, "engineer", clean, { thread, ref })) {
        res.offline.push(t.name);
        continue;
      }
      const why = HOLD_REASONS[this.status.get(t.agent_id)?.status ?? "offline"];
      if (why) res.held.push({ name: t.name, why });
      if (ref) this.watchAcceptance(t, ref);
    }
    return res;
  }

  /** If an agent has not picked a message up in time, say so instead of leaving the human guessing. */
  private watchAcceptance(a: AgentRow, ref: string): void {
    const timer = setTimeout(() => {
      const waiting = [...this.pending.values()].some((p) => p.agentId === a.agent_id && p.ref === ref);
      if (!waiting) return;
      const st = this.status.get(a.agent_id)?.status ?? "offline";
      void this.out.notice(a.project, `${a.name} has not picked up your message yet (${HOLD_REASONS[st] ?? st}). It stays queued.`);
    }, this.acceptTimeoutMs);
    timer.unref();
  }

  /** Agent chat is visible to peers. Mentioned peers get it, otherwise everyone. Loop guard applies. */
  private routeAgentMessage(from: AgentRow, text: string, thread?: string): void {
    const peers = this.db.agentsOfProject(from.project).filter((a) => a.agent_id !== from.agent_id);
    if (!peers.length) return;
    const mentionsHuman = /@engineer\b/i.test(text);
    const mentioned = peers.filter((a) => new RegExp(`(^|\\W)@${escapeRe(a.name)}(\\W|$)`, "i").test(text));
    const n = (this.streak.get(from.project) ?? 0) + 1;
    this.streak.set(from.project, n);
    if (n === this.streakLimit) {
      void this.out.notice(from.project, "Agents have exchanged many messages without input. Pausing forwarding until you reply.", true);
    }
    if (n >= this.streakLimit || mentionsHuman) return;
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
