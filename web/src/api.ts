// Everything the dashboard asks the hub for. One place, so a page never builds a URL or reads a status by hand.

export class ApiError extends Error {
  constructor(public status: number, message: string) {
    super(message)
  }
}

async function call<T>(method: string, path: string, body?: unknown): Promise<T> {
  const res = await fetch(path, {
    method,
    credentials: 'same-origin',
    headers: body === undefined ? undefined : { 'content-type': 'application/json' },
    body: body === undefined ? undefined : JSON.stringify(body),
  })
  const text = await res.text()
  const data = text ? JSON.parse(text) : null
  if (!res.ok) throw new ApiError(res.status, data?.error ?? `${res.status}`)
  return data as T
}

const get = <T,>(p: string) => call<T>('GET', p)
const post = <T,>(p: string, b: unknown) => call<T>('POST', p, b)

export type Me = { id: string; name: string }
export type Bot = { id: string; app_id: string; name: string; invite_url: string }
export type Guild = { id: string; name: string; ok?: boolean; missing?: string[] }
export type Channel = { id: string; name: string }
export type Machine = { node: string; since: number }
export type Project = {
  project: string
  placed: boolean
  bot: string | null
  guild: string | null
  channel: string | null
  guild_name: string | null
  channel_name: string | null
  problem?: string | null
}
export type Device = { node: string; connected: boolean; agents: number; max: number | null; labels: string[]; lastSeen: number | null }
export type Agent = { name: string; lead: boolean; node: string; status: string }
export type ProjectState = {
  agents: Agent[]
  asks: { id: string; agent: string; question: string }[]
  perms: { id: string; agent: string; kind: string; action: string }[]
  tasks: { id: string; to: string; text: string; state: string }[]
}
export type State = { devices: Device[]; projects: Record<string, ProjectState> }

export const api = {
  me: () => get<Me>('/api/v1/me'),
  logout: () => post<null>('/auth/logout', {}),
  state: () => get<State>('/api/v1/state'),
  machines: () => get<Machine[]>('/api/v1/machines'),
  revokeMachine: (node: string) => post<{ revoked: number }>('/api/v1/machines/revoke', { node }),
  lookupCode: (code: string) => get<{ code: string; node: string }>(`/api/device/lookup?code=${encodeURIComponent(code)}`),
  approve: (code: string, node: string) => post<{ ok: boolean }>('/api/device/approve', { code, node }),
  deny: (code: string) => post<{ ok: boolean }>('/api/device/deny', { code }),
  bots: () => get<Bot[]>('/api/v1/bots'),
  addBot: (token: string) => post<Bot>('/api/v1/bots', { token }),
  removeBot: (id: string) => call<{ removed: boolean }>('DELETE', `/api/v1/bots/${id}`),
  guilds: (bot: string) => get<Guild[]>(`/api/v1/bots/${bot}/guilds`),
  channels: (bot: string, guild: string) => get<Channel[]>(`/api/v1/bots/${bot}/guilds/${guild}/channels`),
  makeChannel: (bot: string, guild: string, name: string) =>
    post<Channel>(`/api/v1/bots/${bot}/guilds/${guild}/channels`, { name }),
  projects: () => get<Project[]>('/api/v1/projects'),
  place: (project: string, bot: string, guild: string, channel: string) =>
    call<{ ok: boolean }>('PUT', `/api/v1/projects/${encodeURIComponent(project)}/target`, { bot, guild, channel }),
  pick: (code: string) => get<{ folder: string; node: string }>(`/api/v1/pick/${encodeURIComponent(code)}`),
  choose: (code: string, project: string, agent?: string, adapter?: string) =>
    post<{ ok: boolean }>(`/api/v1/pick/${encodeURIComponent(code)}`, { project, agent: agent || undefined, adapter }),
  spawn: (project: string, name: string, adapter: string, node?: string) =>
    post<{ node: string }>('/api/v1/spawn', { project, name, adapter, node }),
}

export type Summary = {
  since: number
  now: number
  nodes: { name: string; kind: 'agent' | 'human' }[]
  edges: { from: string; to: string; n: number }[]
  lanes: { project: string; agent: string; segments: { state: string; from: number; to: number }[] }[]
  buckets: { t: number; turns: number; tokens: number; messages: number }[]
  cost: { agent: string; turns: number; tokens: number }[]
  asks: { opened: number; answered: number; p50_ms: number | null; p95_ms: number | null }
  tasks: { agent: string; assigned: number; accepted: number; done: number; avg_cycle_ms: number | null }[]
}
export type Health = {
  target: number
  components: Record<string, { state: string; windows: Record<string, { availability: number | null; downMs: number }>; outages: { from: number; to: number }[] }>
}
export type Insights = {
  range_ms: number
  summary: Summary
  pickup: { n: number; p50_ms: number | null; p95_ms: number | null } | null
  health: Health
}
export const getInsights = (range: string) => get<Insights>(`/api/v1/insights?range=${range}`)
