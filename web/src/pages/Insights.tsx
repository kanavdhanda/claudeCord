import { forceCenter, forceLink, forceManyBody, forceSimulation, forceCollide, type SimulationLinkDatum, type SimulationNodeDatum } from 'd3-force'
import { useMemo, useState } from 'react'
import { api, getInsights, type Health, type Summary } from '../api'
import { useLoad } from '../hooks'

const RANGES = ['1h', '24h', '7d', '30d'] as const

/** Insights: how the agents behave. Four graphs (who talks to whom, each agent's state over time, task and question flow, turns and cost) and the numbers behind them. */
export function Insights() {
  const [range, setRange] = useState<(typeof RANGES)[number]>('24h')
  const data = useLoad(() => getInsights(range), 10000, range)
  const bots = useLoad(api.bots)
  const i = data.data
  return (
    <div className="stack">
      <div className="row between">
        <h1>Insights</h1>
        <a className="btn" href="/api/v1/export" download>Download conversation (Obsidian vault)</a>
        <div className="seg">
          {RANGES.map((r) => (
            <button key={r} className={r === range ? 'on' : ''} onClick={() => setRange(r)}>{r}</button>
          ))}
        </div>
      </div>
      {!i ? <p className="muted">Loading…</p> : (
        <>
          <Stats s={i.summary} pickup={i.pickup} />
          <section className="card stack"><h2>Who talks to whom</h2><Network s={i.summary} /></section>
          <section className="card stack"><h2>Agent state over time</h2><Timeline s={i.summary} /></section>
          <section className="card stack"><h2>Tasks and questions</h2><Flow s={i.summary} /></section>
          <section className="card stack"><h2>Turns and estimated input tokens</h2><CostChart s={i.summary} /></section>
          <section className="card stack"><h2>Health</h2><HealthTable h={i.health} botNames={Object.fromEntries((bots.data ?? []).map((b) => [b.id, b.name]))} /></section>
        </>
      )}
    </div>
  )
}

const ms = (v: number | null | undefined) => {
  if (v == null) return '–'
  if (v < 1000) return `${v} ms`
  if (v < 60000) return `${(v / 1000).toFixed(1)} s`
  if (v < 3600000) return `${Math.round(v / 60000)} min`
  return `${(v / 3600000).toFixed(1)} h`
}
const num = (n: number) => (n >= 10000 ? `${(n / 1000).toFixed(1)}k` : String(n))

function Stat({ label, value, hint }: { label: string; value: string; hint?: string }) {
  return (
    <div className="card tight stat">
      <span className="muted small">{label}</span>
      <b className="big">{value}</b>
      {hint && <span className="muted small">{hint}</span>}
    </div>
  )
}

function Stats({ s, pickup }: { s: Summary; pickup: { n: number; p50_ms: number | null; p95_ms: number | null } | null }) {
  const turns = s.cost.reduce((a, c) => a + c.turns, 0)
  const tokens = s.cost.reduce((a, c) => a + c.tokens, 0)
  const messages = s.buckets.reduce((a, b) => a + b.messages, 0)
  const done = s.tasks.reduce((a, t) => a + t.done, 0)
  return (
    <div className="grid stats">
      <Stat label="Question wait (median)" value={ms(s.asks.p50_ms)} hint={`slowest 1 in 20: ${ms(s.asks.p95_ms)}`} />
      <Stat label="Agent picks a message up" value={ms(pickup?.p50_ms)} hint={`slowest 1 in 20: ${ms(pickup?.p95_ms)}`} />
      <Stat label="Turns" value={num(turns)} hint={`≈ ${num(tokens)} input tokens`} />
      <Stat label="Messages" value={num(messages)} />
      <Stat label="Tasks done" value={num(done)} />
      <Stat label="Questions" value={`${s.asks.answered}/${s.asks.opened}`} hint="answered / asked" />
    </div>
  )
}

type N = SimulationNodeDatum & { name: string; kind: 'agent' | 'human' }
type L = SimulationLinkDatum<N> & { n: number }

/** The conversation as a graph: people and agents are dots, a line is messages from one to another, thicker for more. The layout is worked out once, not animated. */
function Network({ s }: { s: Summary }) {
  const W = 760, H = 380
  const layout = useMemo(() => {
    const nodes: N[] = s.nodes.map((n) => ({ ...n }))
    const byName = new Map(nodes.map((n) => [n.name, n]))
    const links: L[] = s.edges.flatMap((e) => {
      const a = byName.get(e.from), b = byName.get(e.to)
      return a && b ? [{ source: a, target: b, n: e.n }] : []
    })
    const sim = forceSimulation<N>(nodes)
      .force('link', forceLink<N, L>(links).distance(110).strength(0.6))
      .force('charge', forceManyBody().strength(-380))
      .force('collide', forceCollide(34))
      .force('center', forceCenter(W / 2, H / 2))
      .stop()
    for (let i = 0; i < 300; i++) sim.tick()
    const clamp = (v: number | undefined, max: number) => Math.max(30, Math.min(max - 30, v ?? max / 2))
    nodes.forEach((n) => { n.x = clamp(n.x, W); n.y = clamp(n.y, H) })
    return { nodes, links }
  }, [s.nodes, s.edges])
  if (s.nodes.length === 0) return <p className="muted">Nothing yet. Edges appear once people and agents have exchanged messages.</p>
  const max = Math.max(1, ...layout.links.map((l) => l.n))
  return (
    <svg viewBox={`0 0 ${W} ${H}`} className="graph" role="img" aria-label="Messages between people and agents">
      <defs>
        <marker id="arrow" viewBox="0 0 10 10" refX="9" refY="5" markerWidth="6" markerHeight="6" orient="auto-start-reverse">
          <path d="M0 0L10 5L0 10z" className="arrowhead" />
        </marker>
      </defs>
      {layout.links.map((l, i) => {
        const a = l.source as N, b = l.target as N
        return (
          <g key={i}>
            <line x1={a.x} y1={a.y} x2={b.x} y2={b.y} className="edge" strokeWidth={1 + (4 * l.n) / max} markerEnd="url(#arrow)" />
            <text x={((a.x ?? 0) + (b.x ?? 0)) / 2} y={((a.y ?? 0) + (b.y ?? 0)) / 2 - 4} className="edge-label">{l.n}</text>
          </g>
        )
      })}
      {layout.nodes.map((n) => (
        <g key={n.name} transform={`translate(${n.x},${n.y})`}>
          <circle r={14} className={n.kind === 'agent' ? 'node agent' : 'node human'} />
          <text y={30} textAnchor="middle" className="node-label">{n.name}</text>
        </g>
      ))}
    </svg>
  )
}

const STATE_CLASS: Record<string, string> = {
  idle: 's-idle', thinking: 's-busy', executing: 's-busy', waiting_input: 's-wait', limited: 's-limit',
  paused: 's-off', offline: 's-off', starting: 's-start',
}

/** One row per agent; each stretch of time is coloured by what the agent was doing. */
function Timeline({ s }: { s: Summary }) {
  const W = 760, LABEL = 110, ROW = 26
  if (s.lanes.length === 0) return <p className="muted">No agent has reported a state in this period.</p>
  const span = Math.max(1, s.now - s.since)
  const x = (t: number) => LABEL + ((Math.max(s.since, t) - s.since) / span) * (W - LABEL - 8)
  const H = s.lanes.length * ROW + 24
  const used = [...new Set(s.lanes.flatMap((l) => l.segments.map((g) => g.state)))]
  return (
    <>
      <svg viewBox={`0 0 ${W} ${H}`} className="graph" role="img" aria-label="Each agent's state over time">
        {s.lanes.map((l, r) => (
          <g key={`${l.project}/${l.agent}`} transform={`translate(0,${r * ROW + 4})`}>
            <text x={LABEL - 8} y={16} textAnchor="end" className="node-label">{l.agent}</text>
            {l.segments.map((g, i) => (
              <rect key={i} x={x(g.from)} y={2} width={Math.max(1.5, x(g.to) - x(g.from))} height={ROW - 8} rx={3} className={STATE_CLASS[g.state] ?? 's-off'}>
                <title>{g.state}: {new Date(g.from).toLocaleString()} for {ms(g.to - g.from)}</title>
              </rect>
            ))}
          </g>
        ))}
        <text x={LABEL} y={H - 4} className="axis">{new Date(s.since).toLocaleString()}</text>
        <text x={W - 8} y={H - 4} textAnchor="end" className="axis">now</text>
      </svg>
      <div className="legend">{used.map((u) => <span key={u}><i className={`sw ${STATE_CLASS[u] ?? 's-off'}`} /> {u.replace('_', ' ')}</span>)}</div>
    </>
  )
}

/** Work as it moves: tasks handed out, picked up and finished per agent, and how long questions waited for an answer. */
function Flow({ s }: { s: Summary }) {
  const max = Math.max(1, ...s.tasks.map((t) => t.assigned))
  return (
    <div className="stack">
      {s.tasks.length === 0 ? <p className="muted">No tasks in this period.</p> : (
        <table className="flow">
          <thead><tr><th>Agent</th><th>Assigned → accepted → done</th><th>Average time to finish</th></tr></thead>
          <tbody>
            {s.tasks.map((t) => (
              <tr key={t.agent}>
                <td><b>{t.agent}</b></td>
                <td>
                  <svg viewBox="0 0 300 40" className="bars" role="img" aria-label={`${t.assigned} assigned, ${t.accepted} accepted, ${t.done} done`}>
                    {[['assigned', t.assigned, 'f-a'], ['accepted', t.accepted, 'f-b'], ['done', t.done, 'f-c']].map(([label, v, cls], k) => (
                      <g key={label as string} transform={`translate(0,${k * 13})`}>
                        <rect width={Math.max(2, ((v as number) / max) * 230)} height={10} rx={2} className={cls as string} />
                        <text x={Math.max(2, ((v as number) / max) * 230) + 6} y={9} className="axis">{label} {v as number}</text>
                      </g>
                    ))}
                  </svg>
                </td>
                <td>{ms(t.avg_cycle_ms)}</td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
      <p className="small muted">
        Questions: {s.asks.answered} of {s.asks.opened} answered · median wait {ms(s.asks.p50_ms)} · slowest 1 in 20 {ms(s.asks.p95_ms)}
        {s.asks.opened > s.asks.answered && <span className="warn"> · {s.asks.opened - s.asks.answered} waiting on a person</span>}
      </p>
    </div>
  )
}

/** Turns per period as bars, with the estimated input tokens beside them; every wake-up of an agent re-reads the conversation, so turns are the cost that matters. */
function CostChart({ s }: { s: Summary }) {
  const W = 760, H = 190, L = 36, B = 22
  const max = Math.max(1, ...s.buckets.map((b) => b.turns))
  const bw = (W - L - 8) / s.buckets.length
  if (s.cost.length === 0) return <p className="muted">No agent has been woken in this period.</p>
  return (
    <>
      <svg viewBox={`0 0 ${W} ${H}`} className="graph" role="img" aria-label="Turns over time">
        <text x={L - 6} y={14} textAnchor="end" className="axis">{max}</text>
        <text x={L - 6} y={H - B} textAnchor="end" className="axis">0</text>
        <line x1={L} y1={H - B} x2={W - 8} y2={H - B} className="axisline" />
        {s.buckets.map((b, i) => {
          const h = (b.turns / max) * (H - B - 16)
          return (
            <rect key={i} x={L + i * bw + 1} y={H - B - h} width={Math.max(1, bw - 2)} height={h} className="f-a">
              <title>{new Date(b.t).toLocaleString()}: {b.turns} turns, ≈{b.tokens} tokens</title>
            </rect>
          )
        })}
        <text x={L} y={H - 4} className="axis">{new Date(s.since).toLocaleString()}</text>
        <text x={W - 8} y={H - 4} textAnchor="end" className="axis">now</text>
      </svg>
      <table className="flow">
        <thead><tr><th>Agent</th><th>Turns</th><th>≈ Input tokens</th></tr></thead>
        <tbody>{s.cost.map((c) => <tr key={c.agent}><td><b>{c.agent}</b></td><td>{c.turns}</td><td>{num(c.tokens)}</td></tr>)}</tbody>
      </table>
      <p className="small muted">The hub cannot see a model's real token counts (agents run in their own terminals), so tokens are the characters delivered divided by four.</p>
    </>
  )
}

function HealthTable({ h, botNames }: { h: Health; botNames: Record<string, string> }) {
  const rows = Object.entries(h.components ?? {})
  if (rows.length === 0) return <p className="muted">No health records yet.</p>
  const label = (c: string) => (c.startsWith('discord:') ? `Discord bridge: ${botNames[c.slice(8)] ?? c.slice(8)}` : c === 'hub' ? 'Your hub' : c)
  const pct = (v: number | null | undefined) => (v == null ? '–' : `${(v * 100).toFixed(2)}%`)
  return (
    <table className="flow">
      <thead><tr><th>Part</th><th>Now</th><th>Last hour</th><th>Last day</th><th>Last week</th></tr></thead>
      <tbody>
        {rows.map(([c, v]) => (
          <tr key={c}>
            <td><b>{label(c)}</b></td>
            <td><span className={`pill ${v.state === 'up' ? 'ok' : 'warn'}`}>{v.state}</span></td>
            <td>{pct(v.windows['1h']?.availability)}</td>
            <td>{pct(v.windows['24h']?.availability)}</td>
            <td>{pct(v.windows['7d']?.availability)}</td>
          </tr>
        ))}
      </tbody>
    </table>
  )
}
