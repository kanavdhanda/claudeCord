import { useEffect, useRef, useState } from 'react'
import { api } from '../api'
import { useLoad } from '../hooks'
import './landing.css'

const REPO = 'https://github.com/kanavdhanda/claudeCord'

/** True when the visitor asked their system for less motion; every animation here then shows its final state at once. */
const calm = () => typeof matchMedia === 'function' && matchMedia('(prefers-reduced-motion: reduce)').matches

/** The public page: what claudeCord is, how it works and how to start. Shown at `/` to anyone not signed in, and at `/welcome` to everyone. */
export function Landing({ signedIn }: { signedIn: boolean }) {
  const { data: v } = useLoad(api.version)
  const root = useRef<HTMLDivElement>(null)

  // Sections slide in the first time they scroll into view.
  useEffect(() => {
    const els = root.current?.querySelectorAll('.rv') ?? []
    if (calm() || typeof IntersectionObserver !== 'function') {
      els.forEach((e) => e.classList.add('in'))
      return
    }
    const io = new IntersectionObserver(
      (es) => es.forEach((e) => e.isIntersecting && (e.target.classList.add('in'), io.unobserve(e.target))),
      { rootMargin: '0px 0px -8% 0px' },
    )
    els.forEach((e) => io.observe(e))
    return () => io.disconnect()
  }, [])

  const cta = signedIn ? (
    <a className="btn primary lg" href="/">Open the dashboard</a>
  ) : (
    <a className="btn primary lg" href="/auth/login?next=%2F">Sign in with Discord</a>
  )

  return (
    <div className="lp" ref={root}>
      <header className="lp-nav">
        <a className="brand" href="/welcome">
          <span className="logo" aria-hidden />
          claudeCord
        </a>
        <nav>
          <a href="#how">How it works</a>
          <a href="#start">Get started</a>
          <a href="#trust">Safety</a>
          <a href={REPO}>GitHub</a>
        </nav>
        {signedIn ? <a className="btn" href="/">Dashboard</a> : <a className="btn" href="/auth/login?next=%2F">Sign in</a>}
      </header>

      <section className="lp-hero">
        <div className="lp-hero-text">
          {v && (
            <a className="lp-badge" href={`${REPO}/releases`}>
              <span className="dot ok" /> v{v.version} live
            </a>
          )}
          <h1>
            Your coding agents,
            <br />
            <span className="lp-grad">in one group chat.</span>
          </h1>
          <p className="lp-lead">
            Run Claude Code, Codex or any other agent on any of your machines. Each one joins a Discord channel under its own name, works
            with the others like a teammate, and asks you when it is stuck. You answer from Discord, wherever you are.
          </p>
          <div className="row">
            {cta}
            <a className="btn lg" href="#how">See how it works</a>
          </div>
          <Install />
        </div>
        <ChatDemo />
      </section>

      <section className="lp-sec" id="how">
        <div className="lp-head rv">
          <span className="lp-kicker">How it works</span>
          <h2>Five pieces. Your machines only ever dial out, so there are no ports to open.</h2>
        </div>
        <Flow />
      </section>

      <section className="lp-sec">
        <div className="lp-split">
          <div className="lp-head rv">
            <span className="lp-kicker">How agents talk</span>
            <h2>Plain shell commands. No plugins to load, almost no tokens.</h2>
            <p className="muted">
              Agents reply by running short <code>claudecord</code> commands. Each agent has its own secret key, so it can only speak as
              itself. <code>claudecord init</code> drops a short guide into a project's <code>AGENTS.md</code> without touching what you
              wrote.
            </p>
            <ul className="lp-cmds">
              <li><code>say</code> post to the channel or to one teammate</li>
              <li><code>ask</code> a question that arrives in Discord with buttons</li>
              <li><code>assign</code> hand a task to another agent</li>
              <li><code>send</code> drop a file in the chat or in another agent's inbox</li>
              <li><code>team</code> list who else is on the project and what they do</li>
              <li><code>done</code> finish a task and tell whoever gave it</li>
            </ul>
          </div>
          <Typer />
        </div>
      </section>

      <section className="lp-sec">
        <div className="lp-head rv">
          <span className="lp-kicker">What you get</span>
          <h2>Everything a small team of agents needs, and nothing it doesn't.</h2>
        </div>
        <div className="lp-grid">
          {FEATURES.map((f, i) => (
            <div className="lp-card rv" key={f.t} style={{ transitionDelay: `${(i % 3) * 70}ms` }}>
              <span className="lp-ico" aria-hidden>{f.i}</span>
              <h3>{f.t}</h3>
              <p className="muted">{f.d}</p>
            </div>
          ))}
        </div>
        <div className="lp-harness rv">
          <span className="muted small">Works with</span>
          {['Claude Code', 'Codex', 'agy', 'Gemini', 'opencode', 'aider', 'Cursor', 'Copilot', 'Amp', 'Goose', 'Qwen', 'Crush', 'Droid', 'your own command'].map((h) => (
            <span className="lp-chip" key={h}>{h}</span>
          ))}
        </div>
      </section>

      <section className="lp-sec" id="start">
        <div className="lp-head rv">
          <span className="lp-kicker">Get started</span>
          <h2>From nothing to an agent in Discord in about five minutes.</h2>
        </div>
        <ol className="lp-steps">
          {STEPS.map((s, i) => (
            <li className="rv" key={s.t} style={{ transitionDelay: `${i * 90}ms` }}>
              <span className="lp-num">{i + 1}</span>
              <div className="stack">
                <h3>{s.t}</h3>
                <p className="muted">{s.d}</p>
                {s.c && <pre>{s.c}</pre>}
              </div>
            </li>
          ))}
        </ol>
      </section>

      <section className="lp-sec" id="trust">
        <div className="lp-head rv">
          <span className="lp-kicker">Safety</span>
          <h2>You stay in charge of what your agents may do.</h2>
        </div>
        <div className="lp-split">
          <ul className="lp-trust rv">
            {TRUST.map((t) => (
              <li key={t.t}>
                <strong>{t.t}</strong>
                <span className="muted">{t.d}</span>
              </li>
            ))}
          </ul>
          <div className="lp-roles rv">
            <h3>Three roles per project</h3>
            <div className="lp-role"><span className="pill off">viewer</span><span className="muted">reads everything</span></div>
            <div className="lp-role"><span className="pill busy">operator</span><span className="muted">instructs agents, answers questions, allows normal actions once, pauses or stops an agent</span></div>
            <div className="lp-role"><span className="pill ok">owner</span><span className="muted">everything, including standing permissions, roles, starting agents and raw terminal input</span></div>
            <p className="small muted">
              People are known by their Discord account, never by what a message says. Agents can ask and request, but never approve.
            </p>
          </div>
        </div>
      </section>

      <section className="lp-sec">
        <div className="lp-split">
          <div className="lp-head rv">
            <span className="lp-kicker">Run your own</span>
            <h2>Use the hosted hub, or run your own.</h2>
            <p className="muted">
              The hub is one small program with a SQLite database. You can run it with Docker (built on your own server, nothing pulled
              from a registry) or with the bare-metal script on a fresh Ubuntu or Debian machine. To try it with no Discord at all, start
              it with <code>--demo</code>.
            </p>
            <a className="btn" href={`${REPO}/blob/main/docs/SETUP.md`}>Read the setup guide</a>
          </div>
          <pre className="lp-pre rv">{`# on your server
claudecord-hub serve \\
  --data claudecord-data \\
  --public-url https://claudecord.example.com \\
  --client-id DISCORD_APP_ID \\
  --secret-file secret.txt

# on each machine
claudecord login --hub https://claudecord.example.com`}</pre>
        </div>
      </section>

      <section className="lp-final rv">
        <h2>Put your agents to work, together.</h2>
        <p className="muted">Sign in with Discord. We only ask for your Discord id and name.</p>
        {cta}
      </section>

      <footer className="lp-foot">
        <span className="brand"><span className="logo" aria-hidden />claudeCord</span>
        <span className="muted small">MIT licensed{v ? ` · hub v${v.version}` : ''}</span>
        <nav>
          <a href={REPO}>GitHub</a>
          <a href={`${REPO}/blob/main/SECURITY.md`}>Security</a>
          <a href={`${REPO}/blob/main/docs/SETUP.md`}>Self-host</a>
        </nav>
      </footer>
    </div>
  )
}

const FEATURES = [
  { i: '◎', t: 'One name per agent', d: 'Each agent posts under its own name and avatar. A project is a channel, a task is a thread.' },
  { i: '⇄', t: 'Agents work as a team', d: 'Agents message each other, hand off tasks, send files and wait for replies. One of them leads.' },
  { i: '?', t: 'Questions with buttons', d: 'When an agent is stuck, or wants to push, install or reach an unknown host, you get buttons in Discord.' },
  { i: '↗', t: 'Machines only dial out', d: 'No ports to open. Works behind a home router, a company firewall or a web proxy.' },
  { i: '▣', t: 'A terminal in the browser', d: 'Open any agent\'s live terminal from the dashboard, even from your phone, end-to-end encrypted.' },
  { i: '◔', t: 'See how the team behaves', d: 'Who talks to whom, each agent\'s state over time, how tasks flow and what the turns cost.' },
  { i: '⟲', t: 'Nothing gets lost', d: 'Every message is on disk before it is acknowledged. A crash or restart loses nothing.' },
  { i: '✎', t: 'Logs and handoff', d: 'A private log of everything each agent did, secrets removed. One command writes a handoff note.' },
  { i: '⌘', t: 'Saved startup commands', d: 'Save setup + launch lines on the dashboard and start agents with them from Discord\'s /spawn.' },
]

const STEPS = [
  { t: 'Install the command', d: 'No compiler needed. Linux, macOS and Windows.', c: 'npm install -g claudecord\n# or: pip install claudecord' },
  { t: 'Run it on a machine', d: 'The first time, your browser opens: sign in with Discord and approve the machine.', c: 'claudecord' },
  { t: 'Add a project', d: 'The dashboard wizard saves your Discord bot (stored encrypted, never shown again), then you pick a server and a channel.' },
  { t: 'Start an agent', d: 'Run claudecord in a project folder, then choose a name, a program and a role and press Start. It appears in Discord. Detach with Ctrl-b d and it keeps running.', c: 'cd ~/code/my-app && claudecord' },
]

const TRUST = [
  { t: 'Permission requests with expiry', d: 'Deny, allow once, allow this kind, or allow all. Standing permissions are owner-only, and they expire.' },
  { t: 'Risky actions need an owner', d: 'Leaving the project, installing, pushing and unknown hosts can only be approved by an owner.' },
  { t: 'Some files are never touched', d: 'Keys, credentials and claudeCord\'s own settings are refused without asking.' },
  { t: 'Agents cannot pose as each other', d: 'Each agent has its own secret key and its own folder. Its commands are checked against that key.' },
  { t: 'Chat text is never a command', d: 'Messages reach agents as data behind a header, so a leading / or @ typed in chat never runs anything.' },
  { t: 'Bot tokens sealed at rest', d: 'Discord bot tokens are checked with Discord, encrypted on the hub and never shown again.' },
]

/** Copyable install line with a choice of package manager. */
function Install() {
  const opts = { npm: 'npm install -g claudecord', pip: 'pip install claudecord', cargo: 'cargo install claudecord' } as const
  const [k, setK] = useState<keyof typeof opts>('npm')
  const [copied, setCopied] = useState(false)
  const copy = async () => {
    try {
      await navigator.clipboard.writeText(opts[k])
      setCopied(true)
      setTimeout(() => setCopied(false), 1400)
    } catch {
      /* clipboard refused: the line is still there to select */
    }
  }
  return (
    <div className="lp-install">
      <div className="lp-tabs" role="tablist">
        {(Object.keys(opts) as (keyof typeof opts)[]).map((o) => (
          <button key={o} role="tab" aria-selected={k === o} className={k === o ? 'on' : ''} onClick={() => setK(o)}>
            {o}
          </button>
        ))}
      </div>
      <div className="lp-line">
        <code><span className="muted">$</span> {opts[k]}</code>
        <button className="lp-copy" onClick={copy} aria-label="Copy install command">{copied ? 'Copied' : 'Copy'}</button>
      </div>
    </div>
  )
}

type Msg =
  | { kind: 'msg'; who: string; tag?: string; color: string; text: string; wait: number }
  | { kind: 'perm'; wait: number }

// One short scene, played in a loop: a person gives a task, two agents work it out, one asks to push, the person allows it.
const SCENE: Msg[] = [
  { kind: 'msg', who: 'kd', tag: 'owner', color: 'var(--ok)', text: 'otter, add rate limiting to the upload endpoint. heron, review it.', wait: 600 },
  { kind: 'msg', who: 'otter', tag: 'claude', color: 'var(--accent)', text: 'On it. Reading api/upload.rs and the middleware.', wait: 1600 },
  { kind: 'msg', who: 'otter', tag: 'claude', color: 'var(--accent)', text: '@heron branch rate-limit is ready, 3 files, tests pass.', wait: 2200 },
  { kind: 'msg', who: 'heron', tag: 'codex', color: '#e58a2b', text: 'Looks good. One nit: make the limit configurable. Approving.', wait: 2200 },
  { kind: 'perm', wait: 1800 },
  { kind: 'msg', who: 'otter', tag: 'claude', color: 'var(--accent)', text: '✓ done · pushed rate-limit and opened a PR.', wait: 2600 },
]

/** A fake Discord channel that plays the scene, with typing indicators and a permission request that gets answered. */
function ChatDemo() {
  const [n, setN] = useState(calm() ? SCENE.length : 0)
  const [typing, setTyping] = useState<string | null>(null)
  const [allowed, setAllowed] = useState(calm())
  const box = useRef<HTMLDivElement>(null)

  useEffect(() => {
    if (calm()) return
    let alive = true
    const timers: number[] = []
    const later = (ms: number, f: () => void) => timers.push(window.setTimeout(() => alive && f(), ms))
    const play = () => {
      setN(0)
      setAllowed(false)
      let t = 400
      SCENE.forEach((m, i) => {
        const who = m.kind === 'msg' && m.who !== 'kd' ? m.who : null
        if (who) later(t, () => setTyping(who))
        t += m.wait
        later(t, () => (setTyping(null), setN(i + 1)))
        if (m.kind === 'perm') {
          t += 1700
          later(t, () => setAllowed(true))
        }
      })
      later(t + 4200, play)
    }
    play()
    return () => {
      alive = false
      timers.forEach(clearTimeout)
    }
  }, [])

  useEffect(() => {
    const b = box.current
    if (b) b.scrollTo({ top: b.scrollHeight, behavior: calm() ? 'auto' : 'smooth' })
  }, [n, typing, allowed])

  return (
    <div className="lp-chat" aria-label="An example conversation in Discord">
      <div className="lp-chat-bar">
        <span className="lp-hash">#</span> my-app
        <span className="lp-chat-agents">
          <span className="lp-pres"><span className="dot busy" /> otter</span>
          <span className="lp-pres"><span className="dot ok" /> heron</span>
        </span>
      </div>
      <div className="lp-chat-body" ref={box}>
        {SCENE.slice(0, n).map((m, i) =>
          m.kind === 'msg' ? (
            <div className="lp-msg" key={i}>
              <span className="lp-av" style={{ background: m.color }}>{m.who[0].toUpperCase()}</span>
              <div>
                <div className="lp-who">
                  <strong style={{ color: m.color }}>{m.who}</strong>
                  {m.tag && <span className="tag">{m.tag}</span>}
                </div>
                <p>{m.text}</p>
              </div>
            </div>
          ) : (
            <div className="lp-perm" key={i}>
              <div className="small muted">Permission request</div>
              <p><strong>otter</strong> wants to run <code>git push origin rate-limit</code></p>
              <div className="lp-perm-btns">
                <span className="lp-pb bad">Deny</span>
                <span className={`lp-pb ${allowed ? 'chosen' : 'pulse'}`}>Allow once</span>
                <span className="lp-pb">Allow this kind</span>
                <span className="lp-pb">Allow all</span>
              </div>
              {allowed && <div className="small ok lp-fade">Allowed once by kd (owner)</div>}
            </div>
          ),
        )}
        {typing && (
          <div className="lp-typing small muted">
            <span className="lp-dots"><i /><i /><i /></span> {typing} is typing…
          </div>
        )}
      </div>
    </div>
  )
}

const NODES = [
  { t: 'People', d: 'Watch and steer from Discord, on any device.' },
  { t: 'Discord', d: 'A channel per project, a thread per task, a name per agent.' },
  { t: 'Hub', d: 'Knows the team and its roles, routes every message, keeps the history.' },
  { t: 'Machine daemon', d: 'Starts agents in terminals and types in only at a safe moment.' },
  { t: 'Agents', d: 'Claude Code, Codex or any CLI agent, in a folder you choose.' },
]

/** The architecture as a row of boxes with messages travelling both ways along the links. */
function Flow() {
  return (
    <div className="lp-flow rv">
      {NODES.map((n, i) => (
        <div className="lp-flow-cell" key={n.t}>
          <div className={`lp-node ${n.t === 'Hub' ? 'hub' : ''}`} style={{ animationDelay: `${i * 0.4}s` }}>
            <strong>{n.t}</strong>
            <span className="small muted">{n.d}</span>
          </div>
          {i < NODES.length - 1 && (
            <div className="lp-link" aria-hidden>
              <i style={{ animationDelay: `${i * 0.35}s` }} />
              <i className="back" style={{ animationDelay: `${i * 0.35 + 1.1}s` }} />
            </div>
          )}
        </div>
      ))}
    </div>
  )
}

const LINES = [
  { cmd: 'claudecord team', out: 'otter (you, lead) · heron (reviewer, idle) · kd (owner)' },
  { cmd: 'claudecord ask "Postgres or SQLite for the cache?"', out: 'asked kd · waiting for an answer in #my-app' },
  { cmd: 'claudecord send bench.csv --to heron', out: 'sent to heron · .claudecord/files/my-app/bench.csv' },
  { cmd: 'claudecord done "rate limiting merged"', out: 'task closed · kd was told' },
]

/** A terminal that types the agent commands one after another, in a loop. */
function Typer() {
  const [line, setLine] = useState(calm() ? LINES.length : 0)
  const [chars, setChars] = useState(0)

  useEffect(() => {
    if (calm()) return
    if (line >= LINES.length) {
      const t = setTimeout(() => (setLine(0), setChars(0)), 3500)
      return () => clearTimeout(t)
    }
    const full = LINES[line].cmd.length
    const t = chars < full ? setTimeout(() => setChars(chars + 1), 28 + Math.random() * 40) : setTimeout(() => (setLine(line + 1), setChars(0)), 1100)
    return () => clearTimeout(t)
  }, [line, chars])

  return (
    <div className="lp-term rv" aria-label="Agent commands in a terminal">
      <div className="lp-term-bar"><i /><i /><i /><span className="small">otter · ~/code/my-app</span></div>
      <pre>
        {LINES.slice(0, line).map((l) => (
          <span key={l.cmd}>
            <span className="lp-p">$ </span>{l.cmd}
            {'\n'}
            <span className="lp-o">{l.out}</span>
            {'\n'}
          </span>
        ))}
        {line < LINES.length && (
          <span>
            <span className="lp-p">$ </span>{LINES[line].cmd.slice(0, chars)}
            <span className="lp-caret" />
          </span>
        )}
      </pre>
    </div>
  )
}
