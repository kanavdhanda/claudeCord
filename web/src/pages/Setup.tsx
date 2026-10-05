import { useEffect, useState } from 'react'
import { Link } from 'react-router-dom'
import { api, type Bot, type Channel, type Guild } from '../api'
import { useAction, useLoad } from '../hooks'

type Step = 'bot' | 'where' | 'name' | 'done'
const steps: { id: Step; label: string }[] = [
  { id: 'bot', label: 'Discord bot' },
  { id: 'where', label: 'Where' },
  { id: 'name', label: 'Name it' },
  { id: 'done', label: 'Start' },
]

/** The setup wizard: pick (or add) a bot, pick the server and channel (or make one), name the project and agent. It is also how a new project is spawned later. */
export function Setup() {
  const [step, setStep] = useState<Step>('bot')
  const bots = useLoad(api.bots)
  const [bot, setBot] = useState<Bot | null>(null)
  const [guild, setGuild] = useState<Guild | null>(null)
  const [channel, setChannel] = useState<Channel | null>(null)
  const [project, setProject] = useState('')
  const [agent, setAgent] = useState('')
  const [program, setProgram] = useState('claude')

  return (
    <div className="stack">
      <h1>Add a project</h1>
      <ol className="steps">
        {steps.map((s, i) => (
          <li key={s.id} className={s.id === step ? 'on' : steps.findIndex((x) => x.id === step) > i ? 'past' : ''}>
            <span>{i + 1}</span> {s.label}
          </li>
        ))}
      </ol>

      {step === 'bot' && <BotStep bots={bots.data ?? []} reload={bots.reload} pick={(b) => { setBot(b); setStep('where') }} />}
      {step === 'where' && bot && (
        <WhereStep
          bot={bot}
          back={() => setStep('bot')}
          pick={(g, c) => {
            setGuild(g)
            setChannel(c)
            setProject((p) => p || c.name.replace(/[^A-Za-z0-9._-]/g, '-').replace(/^[^A-Za-z0-9]+/, ''))
            setStep('name')
          }}
        />
      )}
      {step === 'name' && bot && guild && channel && (
        <NameStep
          bot={bot}
          guild={guild}
          channel={channel}
          project={project}
          setProject={setProject}
          agent={agent}
          setAgent={setAgent}
          program={program}
          setProgram={setProgram}
          back={() => setStep('where')}
          next={() => setStep('done')}
        />
      )}
      {step === 'done' && <DoneStep project={project} agent={agent} program={program} />}
    </div>
  )
}

function BotStep({ bots, reload, pick }: { bots: Bot[]; reload: () => void; pick: (b: Bot) => void }) {
  const [token, setToken] = useState('')
  const add = useAction()
  return (
    <div className="card stack">
      <h2>Which Discord bot?</h2>
      <p className="muted">
        Agents post through a bot you own, so you control its name, avatar and servers. Create one in the{' '}
        <a href="https://discord.com/developers/applications" target="_blank" rel="noreferrer">Discord developer portal</a> (Bot → Reset token),
        and turn on <b>Message Content Intent</b>. One bot can be in as many servers as you like.
      </p>
      {bots.length > 0 && (
        <div className="list">
          {bots.map((b) => (
            <div key={b.id} className="item">
              <div>
                <b>{b.name}</b> <span className="muted small mono">{b.app_id}</span>
              </div>
              <button className="btn primary" onClick={() => pick(b)}>Use this bot</button>
            </div>
          ))}
        </div>
      )}
      <label>
        {bots.length ? 'Or add another bot token' : 'Paste the bot token'}
        <input type="password" autoComplete="off" value={token} placeholder="Bot token" onChange={(e) => setToken(e.target.value)} />
      </label>
      <p className="small muted">It is checked with Discord, then stored encrypted and never shown again.</p>
      {add.error && <p className="error">{add.error}</p>}
      <div className="row">
        <button
          className="btn primary"
          disabled={add.busy || !token}
          onClick={() =>
            add.run(async () => {
              const b = await api.addBot(token)
              setToken('')
              reload()
              pick(b)
            })
          }
        >
          Save and continue
        </button>
      </div>
    </div>
  )
}

function WhereStep({ bot, back, pick }: { bot: Bot; back: () => void; pick: (g: Guild, c: Channel) => void }) {
  const guilds = useLoad(() => api.guilds(bot.id))
  const [guildId, setGuildId] = useState('')
  const [channels, setChannels] = useState<Channel[]>([])
  const [mode, setMode] = useState<'existing' | 'new'>('new')
  const [channelId, setChannelId] = useState('')
  const [name, setName] = useState('')
  const go = useAction()
  const guild = guilds.data?.find((g) => g.id === guildId) ?? null

  useEffect(() => {
    if (!guildId) return
    api.channels(bot.id, guildId).then((c) => { setChannels(c); setChannelId(c[0]?.id ?? '') }).catch(() => setChannels([]))
  }, [bot.id, guildId])

  return (
    <div className="card stack">
      <h2>Where should it live?</h2>
      {guilds.data && guilds.data.length === 0 && (
        <p className="warn">
          <b>{bot.name}</b> is not in any server yet. <a href={bot.invite_url} target="_blank" rel="noreferrer">Add it to a server</a>, then{' '}
          <button className="link" onClick={guilds.reload}>check again</button>.
        </p>
      )}
      <label>
        Discord server
        <select value={guildId} onChange={(e) => setGuildId(e.target.value)}>
          <option value="">Choose a server…</option>
          {guilds.data?.map((g) => <option key={g.id} value={g.id}>{g.name}</option>)}
        </select>
      </label>
      <p className="small muted">
        Not listed? <a href={bot.invite_url} target="_blank" rel="noreferrer">Add the bot to another server</a>, then{' '}
        <button className="link" onClick={guilds.reload}>refresh</button>.
      </p>
      {guild && (
        <>
          <div className="seg">
            <button className={mode === 'new' ? 'on' : ''} onClick={() => setMode('new')}>Create a new channel</button>
            <button className={mode === 'existing' ? 'on' : ''} onClick={() => setMode('existing')}>Use an existing channel</button>
          </div>
          {mode === 'new' ? (
            <label>
              Channel name
              <input value={name} placeholder="my-project" onChange={(e) => setName(e.target.value)} />
            </label>
          ) : (
            <label>
              Channel
              <select value={channelId} onChange={(e) => setChannelId(e.target.value)}>
                {channels.map((c) => <option key={c.id} value={c.id}>#{c.name}</option>)}
              </select>
            </label>
          )}
        </>
      )}
      {go.error && <p className="error">{go.error}</p>}
      <div className="row">
        <button className="btn" onClick={back}>Back</button>
        <button
          className="btn primary"
          disabled={go.busy || !guild || (mode === 'new' ? !name : !channelId)}
          onClick={() =>
            go.run(async () => {
              if (!guild) return
              if (mode === 'new') {
                const c = await api.makeChannel(bot.id, guild.id, name)
                pick(guild, c)
              } else {
                const c = channels.find((x) => x.id === channelId)
                if (c) pick(guild, c)
              }
            })
          }
        >
          Continue
        </button>
      </div>
    </div>
  )
}

function NameStep(p: {
  bot: Bot; guild: Guild; channel: Channel; project: string; setProject: (s: string) => void
  agent: string; setAgent: (s: string) => void; program: string; setProgram: (s: string) => void
  back: () => void; next: () => void
}) {
  const save = useAction()
  const slug = /^[A-Za-z0-9][A-Za-z0-9._-]{0,49}$/
  const ok = slug.test(p.project) && (p.agent === '' || slug.test(p.agent))
  return (
    <div className="card stack">
      <h2>Name it</h2>
      <p className="muted">
        Posting to <b>#{p.channel.name}</b> in <b>{p.guild.name}</b> with <b>{p.bot.name}</b>.
      </p>
      <label>
        Project name
        <input value={p.project} onChange={(e) => p.setProject(e.target.value)} />
      </label>
      <label>
        First agent's name <span className="muted small">(optional; a friendly one is chosen if you leave it empty)</span>
        <input value={p.agent} placeholder="otter" onChange={(e) => p.setAgent(e.target.value)} />
      </label>
      <label>
        Agent program
        <select value={p.program} onChange={(e) => p.setProgram(e.target.value)}>
          <option value="claude">Claude Code</option>
          <option value="codex">Codex</option>
          <option value="agy">agy</option>
        </select>
      </label>
      {!ok && <p className="error">Names use letters, digits, dots, dashes and underscores.</p>}
      {save.error && <p className="error">{save.error}</p>}
      <div className="row">
        <button className="btn" onClick={p.back}>Back</button>
        <button
          className="btn primary"
          disabled={save.busy || !ok}
          onClick={() =>
            save.run(async () => {
              await api.place(p.project, p.bot.id, p.guild.id, p.channel.id)
              p.next()
            })
          }
        >
          Create
        </button>
      </div>
    </div>
  )
}

function DoneStep({ project, agent, program }: { project: string; agent: string; program: string }) {
  const machines = useLoad(api.machines)
  const [node, setNode] = useState('')
  const spawn = useAction()
  const [started, setStarted] = useState<string | null>(null)
  const cmd = `cd ~/code/${project}\nclaudecord start --project ${project}${agent ? ` --name ${agent}` : ''}${program !== 'claude' ? ` --adapter ${program}` : ''}`
  return (
    <div className="card stack">
      <h2>Start the first agent</h2>
      <p>
        <b>{project}</b> now has its place in Discord. On the machine that has the code, run this in the project folder:
      </p>
      <pre>{cmd}</pre>
      <button className="btn" onClick={() => navigator.clipboard?.writeText(cmd)}>Copy</button>
      {agent && (machines.data?.length ?? 0) > 0 && (
        <div className="stack">
          <h3>Or start another agent from here</h3>
          <p className="small muted">This works for projects whose folder a machine already knows (after the command above has run once there).</p>
          <div className="row">
            <select value={node} onChange={(e) => setNode(e.target.value)}>
              <option value="">Any machine with room</option>
              {machines.data?.map((m) => <option key={m.node} value={m.node}>{m.node}</option>)}
            </select>
            <button
              className="btn"
              disabled={spawn.busy}
              onClick={() => spawn.run(async () => setStarted((await api.spawn(project, agent, program, node || undefined)).node))}
            >
              Spawn {agent}
            </button>
          </div>
          {spawn.error && <p className="error">{spawn.error}</p>}
          {started && <p className="ok">Asked {started} to start {agent}.</p>}
        </div>
      )}
      <div className="row">
        <Link className="btn primary" to="/">Go to the dashboard</Link>
      </div>
    </div>
  )
}
