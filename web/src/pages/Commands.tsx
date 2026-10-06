import { useState } from 'react'
import { api } from '../api'
import { useAction, useLoad } from '../hooks'

const slug = /^[A-Za-z0-9][A-Za-z0-9._-]{0,49}$/

/**
 * The startup commands of this account. A command is any shell line: setup steps and then the agent's launch. It is kept here, shown here, and
 * chosen when an agent is started (the Spawn button, the page `claudecord start` opens, or Discord's /spawn). A machine runs one only after its
 * owner turned that on with `claudecord settings custom-commands on`.
 */
export function Commands() {
  const list = useLoad(api.commands)
  const [name, setName] = useState('')
  const [command, setCommand] = useState('')
  const [program, setProgram] = useState('claude')
  const save = useAction()
  const del = useAction()
  return (
    <div className="stack">
      <h1>Startup commands</h1>
      <p className="muted">
        Any shell line, such as <code>source venv/bin/activate && claude --model opus</code>. Pick one when you start an agent. It runs in the login shell of
        the machine, and only on machines that allowed it: run <code>claudecord settings custom-commands on</code> there.
      </p>
      <div className="card stack">
        {list.data?.length === 0 && <p className="muted">No saved commands yet.</p>}
        {list.data?.map((c) => (
          <div key={c.name} className="item">
            <div>
              <b>{c.name}</b> <span className="pill">{c.program}</span>
              <pre className="mono small" style={{ whiteSpace: 'pre-wrap', margin: '4px 0 0' }}>{c.command}</pre>
            </div>
            <button
              className="btn danger"
              disabled={del.busy}
              onClick={() => confirm(`Remove ${c.name}?`) && del.run(async () => { await api.deleteCommand(c.name); list.reload() })}
            >
              Remove
            </button>
          </div>
        ))}
        {del.error && <p className="error">{del.error}</p>}
      </div>
      <div className="card stack">
        <h2>Add one</h2>
        <label>
          Name
          <input value={name} placeholder="opus" onChange={(e) => setName(e.target.value)} />
        </label>
        <label>
          Command
          <textarea value={command} rows={3} placeholder="source venv/bin/activate && claude --model opus" onChange={(e) => setCommand(e.target.value)} />
        </label>
        <label>
          The program it starts <span className="muted small">(so it is read correctly)</span>
          <select value={program} onChange={(e) => setProgram(e.target.value)}>
            <option value="claude">Claude Code</option>
            <option value="codex">Codex</option>
            <option value="agy">agy</option>
          </select>
        </label>
        {name !== '' && !slug.test(name) && <p className="error">A name is letters, digits, dots, dashes and underscores.</p>}
        {save.error && <p className="error">{save.error}</p>}
        <button
          className="btn primary"
          disabled={save.busy || !slug.test(name) || command.trim() === ''}
          onClick={() => save.run(async () => { await api.saveCommand(name, command, program); setName(''); setCommand(''); list.reload() })}
        >
          Save
        </button>
      </div>
    </div>
  )
}
