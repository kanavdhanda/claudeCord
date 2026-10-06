import { useEffect, useState } from 'react'
import { Link, useSearchParams } from 'react-router-dom'
import { api, type Bot, type Channel, type Project } from '../api'
import { useAction, useLoad } from '../hooks'

const slug = /^[A-Za-z0-9][A-Za-z0-9._-]{0,49}$/
const clean = (s: string) => s.toLowerCase().replace(/[^a-z0-9._-]+/g, '-').replace(/^[^a-z0-9]+/, '').slice(0, 50)

/**
 * The one page `claudecord start` opens for a folder that belongs to no project yet. Everything is asked here, once: the project's name (the Discord
 * channel is named after it, so it is typed nowhere else), where in Discord, and the first agent. One button does it all and releases the terminal.
 */
export function Pick() {
  const [q] = useSearchParams()
  const code = q.get('code') ?? ''
  const what = useLoad(() => api.pick(code))
  const projects = useLoad<Project[]>(() => api.projects())
  const bots = useLoad<Bot[]>(() => api.bots())
  const [project, setProject] = useState('')
  const [token, setToken] = useState('')
  const [botId, setBotId] = useState('')
  const [guildId, setGuildId] = useState('')
  const [existing, setExisting] = useState(false)
  const [channelId, setChannelId] = useState('')
  const [channels, setChannels] = useState<Channel[]>([])
  const [agent, setAgent] = useState('')
  const [role, setRole] = useState('')
  const [program, setProgram] = useState('claude')
  const [done, setDone] = useState<string | null>(null)
  // After Start: the machine is starting the agent. What it says (started, or why not) is shown here.
  const [waiting, setWaiting] = useState(false)
  const [outcome, setOutcome] = useState<{ ok: boolean; message: string } | null>(null)
  const addBot = useAction()
  const go = useAction()

  // Waiting for the machine to say whether the agent started.
  useEffect(() => {
    if (!waiting || outcome) return
    let tries = 0
    const t = setInterval(async () => {
      tries++
      try {
        const r = (await api.pick(code)).result
        if (r) { setOutcome(r); setWaiting(false) }
      } catch { /* try again */ }
      if (tries > 90) { setOutcome({ ok: false, message: 'Your machine has not answered. Is `claudecord start` still running there?' }); setWaiting(false) }
    }, 1000)
    return () => clearInterval(t)
  }, [waiting, outcome])

  // The project this folder already belongs to comes first; a new folder's name is the first guess.
  useEffect(() => { if (what.data && !project) setProject(what.data.project || clean(what.data.folder)) }, [what.data])
  // One bot needs no question.
  useEffect(() => { if (!botId && bots.data?.length) setBotId(bots.data[0].id) }, [bots.data])
  const bot = bots.data?.find((b) => b.id === botId) ?? null
  const guilds = useLoad(() => (bot ? api.guilds(bot.id) : Promise.resolve([])), 0, botId)
  useEffect(() => {
    const ok = guilds.data?.filter((g) => g.ok !== false) ?? []
    if (!guildId && ok.length === 1) setGuildId(ok[0].id)
  }, [guilds.data])
  useEffect(() => {
    if (!bot || !guildId) return
    api.channels(bot.id, guildId).then((c) => { setChannels(c); setChannelId(c[0]?.id ?? '') }).catch(() => setChannels([]))
  }, [botId, guildId])

  const known = projects.data?.find((p) => p.project === project)
  const placed = !!known?.placed
  const nameOk = slug.test(project) && (agent === '' || slug.test(agent))

  if (!code || what.error)
    return (
      <div className="card narrow stack">
        <h2>Start here</h2>
        <p className="error">{what.error ? what.error.message : 'This link has no code.'}</p>
        <p className="muted">Run <span className="mono">claudecord start</span> in the folder again to get a new link.</p>
      </div>
    )

  if (done)
    return (
      <div className="card narrow stack">
        <h2>{outcome ? (outcome.ok ? 'Started' : 'It did not start') : 'Starting…'}</h2>
        {!outcome && <p className="muted">Your choice reached the machine. Waiting for it to start <b>{done}</b>…</p>}
        {outcome?.ok && <p><b>{done}</b> is running. Go back to your terminal: the agent is there.</p>}
        {outcome && !outcome.ok && (
          <>
            <p className="error" style={{ whiteSpace: 'pre-wrap' }}>{outcome.message}</p>
            <p className="muted">Fix that on the machine, then run <span className="mono">claudecord start</span> again.</p>
          </>
        )}
        <Link className="btn primary" to="/">Go to the dashboard</Link>
      </div>
    )

  const ready = placed || (!!bot && !!guildId && (existing ? !!channelId : true))
  const start = () =>
    go.run(async () => {
      if (!placed && bot) {
        const guild = guilds.data?.find((g) => g.id === guildId)
        if (!guild) return
        const channel = existing ? channels.find((c) => c.id === channelId) : await api.makeChannel(bot.id, guild.id, project)
        if (!channel) return
        await api.place(project, bot.id, guild.id, channel.id)
      }
      const saved = program.startsWith('cmd:') ? program.slice(4) : undefined
      await api.choose(code, project, agent, saved ? what.data?.commands?.find((c) => c.name === saved)?.program ?? 'claude' : program, role, saved)
      setDone(project)
      setWaiting(true)
    })

  return (
    <div className="card narrow stack">
      <h2>Start here</h2>
      {what.data && (
        <p className="muted">
          Folder <b className="mono">{what.data.folder}</b> on <b>{what.data.node}</b>.
        </p>
      )}

      <label>
        Project name
        <input value={project} onChange={(e) => setProject(e.target.value)} />
      </label>
      {(projects.data ?? []).length > 0 && (
        <div className="row">
          <span className="muted small">or join:</span>
          {(projects.data ?? []).map((p) => (
            <button key={p.project} className="btn" onClick={() => setProject(p.project)}>{p.project}</button>
          ))}
        </div>
      )}

      {placed ? (
        <p className="muted">
          <b>{project}</b> already has its place in Discord. Agents started here join it.
        </p>
      ) : (
        <>
          {bots.data && bots.data.length === 0 && (
            <>
              <label>
                Discord bot token
                <input type="password" autoComplete="off" value={token} placeholder="Bot token" onChange={(e) => setToken(e.target.value)} />
              </label>
              <p className="small muted">
                Create a bot in the <a href="https://discord.com/developers/applications" target="_blank" rel="noreferrer">Discord developer portal</a>{' '}
                (Bot → Reset token) and turn on <b>Message Content Intent</b>. Discord checks the token; it is stored encrypted.
              </p>
              {addBot.error && <p className="error">{addBot.error}</p>}
              <button
                className="btn"
                disabled={addBot.busy || !token}
                onClick={() => addBot.run(async () => { const b = await api.addBot(token); setToken(''); await bots.reload(); setBotId(b.id) })}
              >
                Check and save the bot
              </button>
            </>
          )}
          {bots.data && bots.data.length > 1 && (
            <label>
              Discord bot
              <select value={botId} onChange={(e) => { setBotId(e.target.value); setGuildId('') }}>
                {bots.data.map((b) => <option key={b.id} value={b.id}>{b.name}</option>)}
              </select>
            </label>
          )}
          {bot && (
            <>
              {guilds.data && guilds.data.length === 0 && (
                <p className="warn">
                  <b>{bot.name}</b> is not in any server yet. <a href={bot.invite_url} target="_blank" rel="noreferrer">Add it to a server</a>, then{' '}
                  <button className="link" onClick={guilds.reload}>check again</button>.
                </p>
              )}
              {guilds.data && guilds.data.length > 0 && (
                <label>
                  Discord server
                  <select value={guildId} onChange={(e) => setGuildId(e.target.value)}>
                    <option value="">Choose a server…</option>
                    {guilds.data.map((g) => (
                      <option key={g.id} value={g.id} disabled={g.ok === false}>
                        {g.name}{g.ok === false ? ' (the bot needs more permissions here)' : ''}
                      </option>
                    ))}
                  </select>
                </label>
              )}
              {guilds.data?.filter((g) => g.ok === false).map((g) => (
                <p key={g.id} className="warn small">
                  In <b>{g.name}</b> the bot is missing: {g.missing?.join(', ')}. <a href={bot.invite_url} target="_blank" rel="noreferrer">Invite it again</a> (remove it from
                  the server first if Discord keeps the old permissions), then <button className="link" onClick={guilds.reload}>check again</button>.
                </p>
              ))}
              {guildId && (
                <>
                  <div className="seg">
                    <button className={!existing ? 'on' : ''} onClick={() => setExisting(false)}>New channel #{project || '…'}</button>
                    <button className={existing ? 'on' : ''} onClick={() => setExisting(true)}>Use an existing channel</button>
                  </div>
                  {existing && (
                    <select value={channelId} onChange={(e) => setChannelId(e.target.value)}>
                      {channels.map((c) => <option key={c.id} value={c.id}>#{c.name}</option>)}
                    </select>
                  )}
                </>
              )}
            </>
          )}
        </>
      )}

      <div className="row">
        <label>
          Agent <span className="muted small">(a friendly name if empty)</span>
          <input value={agent} placeholder="otter" onChange={(e) => setAgent(e.target.value)} />
        </label>
        <label>
          Program
          <select value={program} onChange={(e) => setProgram(e.target.value)}>
            <option value="claude">Claude Code</option>
            <option value="codex">Codex</option>
            <option value="agy">agy</option>
            {(what.data?.commands ?? []).length > 0 && (
              <optgroup label="Saved commands on this machine">
                {what.data!.commands!.map((c) => (
                  <option key={c.name} value={`cmd:${c.name}`}>{c.name} ({c.program}{c.source === 'folder' ? ', this folder' : ''})</option>
                ))}
              </optgroup>
            )}
          </select>
        </label>
      </div>
      <label>
        Role <span className="muted small">(optional, such as lead, reviewer, tests)</span>
        <input value={role} placeholder="lead" onChange={(e) => setRole(e.target.value)} />
      </label>
      {!nameOk && <p className="error">Names use letters, digits, dots, dashes and underscores.</p>}
      {go.error && <p className="error">{go.error}</p>}
      <button className="btn primary" disabled={go.busy || !nameOk || !ready} onClick={start}>
        Start
      </button>
    </div>
  )
}
