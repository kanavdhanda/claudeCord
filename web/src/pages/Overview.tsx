import { useState } from 'react'
import { Link } from 'react-router-dom'
import { api, type Project, type ProjectState } from '../api'
import { ago, useAction, useLoad } from '../hooks'

const dot = (status: string) => (status === 'idle' || status === 'ready' ? 'ok' : status === 'busy' || status === 'working' ? 'busy' : status === 'offline' ? 'off' : 'warn')

/** The front page: machines, and for each project where it lives in Discord, its agents and what is waiting on a person. */
export function Overview() {
  const state = useLoad(api.state, 3000)
  const projects = useLoad(api.projects, 6000)
  const bots = useLoad(api.bots)
  const approved = useLoad(api.machines, 6000)
  if (!state.data || !projects.data) return <p className="muted">Loading…</p>
  const placed = new Map(projects.data.map((p) => [p.project, p]))
  const names = [...new Set([...Object.keys(state.data.projects), ...projects.data.map((p) => p.project)])].sort()
  // Every approved machine, connected or not: one that has just been approved has not connected yet.
  const seen = new Map(state.data.devices.map((d) => [d.node, d]))
  const devices = [
    ...state.data.devices,
    ...(approved.data ?? []).filter((m) => !seen.has(m.node)).map((m) => ({ node: m.node, connected: false, agents: 0, max: null, labels: [], lastSeen: null })),
  ]
  const empty = bots.data && bots.data.length === 0

  return (
    <div className="stack">
      <h1>Overview</h1>
      {empty && (
        <div className="card callout">
          <div>
            <h3>Get started</h3>
            <p className="muted">Connect a Discord bot and pick where your first project lives.</p>
          </div>
          <Link className="btn primary" to="/setup">Add a project</Link>
        </div>
      )}

      <section className="stack">
        <h2>Machines</h2>
        {devices.length === 0 ? (
          <p className="muted">
            No machine has connected yet. On a machine, run <code>npx claudecord</code> and approve it in your browser.
          </p>
        ) : (
          <div className="grid">
            {devices.map((d) => (
              <div key={d.node} className="card tight">
                <div className="row between">
                  <b>{d.node}</b>
                  <span className={`pill ${d.connected ? 'ok' : 'off'}`}>{d.connected ? 'connected' : 'away'}</span>
                </div>
                <div className="muted small">
                  {d.lastSeen == null ? 'approved, not connected yet' : <>{d.agents}{d.max ? ` of ${d.max}` : ''} agents · seen {ago(d.lastSeen)}</>}
                </div>
              </div>
            ))}
          </div>
        )}
      </section>

      <section className="stack">
        <div className="row between">
          <h2>Projects</h2>
          <Link className="btn" to="/setup">Add a project</Link>
        </div>
        {names.length === 0 && <p className="muted">No projects yet.</p>}
        {names.map((n) => (
          <ProjectCard key={n} name={n} place={placed.get(n)} s={state.data!.projects[n]} machines={state.data!.devices.map((d) => d.node)} targets={projects.data!.filter((p) => p.placed && p.project !== n).map((p) => p.project)} />
        ))}
      </section>
    </div>
  )
}

function ProjectCard({ name, place, s, machines, targets }: { name: string; place?: Project; s?: ProjectState; machines: string[]; targets: string[] }) {
  const [spawning, setSpawning] = useState(false)
  const waiting = (s?.asks.length ?? 0) + (s?.perms.length ?? 0)
  return (
    <div className="card stack">
      <div className="row between">
        <div>
          <h3>{name}</h3>
          {place?.placed ? (
            <>
              <span className="muted small">
                #{place.channel_name} in {place.guild_name}
              </span>
              {place.problem && (
                <p className="error small">
                  Discord is not working for this project: {place.problem} <Link to="/bots">Discord bots</Link>
                </p>
              )}
            </>
          ) : (
            <span className="warn small">Not placed in Discord yet — its agents' messages go nowhere. <Link to="/setup">Choose where</Link></span>
          )}
        </div>
        <div className="row">
          {waiting > 0 && <span className="pill warn">{waiting} waiting on you</span>}
          <button className="btn" onClick={() => setSpawning((x) => !x)}>Spawn agent</button>
        </div>
      </div>
      {spawning && <SpawnForm project={name} machines={machines} done={() => setSpawning(false)} />}
      <div className="agents">
        {s?.agents.length ? (
          s.agents.map((a) => (
            <div key={a.name} className="agent">
              <span className={`dot ${dot(a.status)}`} />
              <b>{a.name}</b>
              {a.lead && <span className="tag">lead</span>}
              <span className="muted small">{a.status} on {a.node}</span>
              <MoveAgent project={name} agent={a.name} targets={targets} />
            </div>
          ))
        ) : (
          <span className="muted small">No agents running.</span>
        )}
      </div>
      {s && s.asks.length > 0 && (
        <div className="stack">
          {s.asks.map((a) => (
            <div key={a.id} className="note"><b>{a.agent}</b> asks ({a.id}): {a.question} <span className="muted small">Answer in Discord.</span></div>
          ))}
        </div>
      )}
      {s && s.tasks.length > 0 && (
        <details>
          <summary>{s.tasks.length} task{s.tasks.length === 1 ? '' : 's'}</summary>
          <ul className="tasks">
            {s.tasks.map((t) => (
              <li key={t.id}><span className={`pill ${t.state === 'done' ? 'ok' : 'busy'}`}>{t.state}</span> {t.to}: {t.text}</li>
            ))}
          </ul>
        </details>
      )}
    </div>
  )
}

function SpawnForm({ project, machines, done }: { project: string; machines: string[]; done: () => void }) {
  const [name, setName] = useState('')
  const [adapter, setAdapter] = useState('claude')
  const [node, setNode] = useState('')
  const act = useAction()
  const [result, setResult] = useState<string | null>(null)
  return (
    <div className="inline-form">
      <input placeholder="agent name" value={name} onChange={(e) => setName(e.target.value)} />
      <select value={adapter} onChange={(e) => setAdapter(e.target.value)}>
        <option value="claude">Claude Code</option>
        <option value="codex">Codex</option>
        <option value="agy">agy</option>
      </select>
      <select value={node} onChange={(e) => setNode(e.target.value)}>
        <option value="">Any machine</option>
        {machines.map((m) => <option key={m}>{m}</option>)}
      </select>
      <button className="btn primary" disabled={act.busy || !name} onClick={() => act.run(async () => {
        setResult((await api.spawn(project, name, adapter, node || undefined)).node)
        // Say it was asked, then close by itself.
        setTimeout(done, 2000)
      })}>Start</button>
      <button className="btn" onClick={done}>Close</button>
      {act.error && <span className="error">{act.error}</span>}
      {result && <span className="ok">Asked {result} to start {name}.</span>}
    </div>
  )
}

/** Moves a running agent to another project in place: the same agent and conversation, filed under the other project from now on. */
function MoveAgent({ project, agent, targets }: { project: string; agent: string; targets: string[] }) {
  const [to, setTo] = useState('')
  const move = useAction()
  if (targets.length === 0) return null
  return (
    <span className="row small">
      <select value={to} onChange={(e) => setTo(e.target.value)} aria-label={`Move ${agent} to another project`}>
        <option value="">Move to…</option>
        {targets.map((t) => <option key={t} value={t}>{t}</option>)}
      </select>
      {to && (
        <button className="btn" disabled={move.busy} onClick={() => move.run(async () => { await api.moveAgent(project, agent, to); setTo('') })}>
          Move
        </button>
      )}
      {move.error && <span className="error small">{move.error}</span>}
    </span>
  )
}
