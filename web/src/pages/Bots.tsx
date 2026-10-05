import { useState } from 'react'
import { api } from '../api'
import { useAction, useLoad } from '../hooks'

/** The account's Discord bots. A token is only ever typed in; once saved it is stored encrypted and never shown again. */
export function Bots() {
  const bots = useLoad(api.bots)
  const [token, setToken] = useState('')
  const add = useAction()
  const del = useAction()
  return (
    <div className="stack">
      <h1>Discord bots</h1>
      <p className="muted">One bot can serve any number of Discord servers. Agents appear under their own names through it.</p>
      <div className="card stack">
        {bots.data?.length === 0 && <p className="muted">No bots yet.</p>}
        {bots.data?.map((b) => (
          <div key={b.id} className="item">
            <div>
              <b>{b.name}</b> <span className="muted small mono">{b.app_id}</span>
              <div className="small"><a href={b.invite_url} target="_blank" rel="noreferrer">Add to a server</a></div>
            </div>
            <button
              className="btn danger"
              disabled={del.busy}
              onClick={() => confirm(`Remove ${b.name}? Projects that use it stop posting until you place them again.`) && del.run(async () => { await api.removeBot(b.id); bots.reload() })}
            >
              Remove
            </button>
          </div>
        ))}
      </div>
      <div className="card stack">
        <h3>Add a bot</h3>
        <label>
          Bot token
          <input type="password" autoComplete="off" value={token} onChange={(e) => setToken(e.target.value)} />
        </label>
        {add.error && <p className="error">{add.error}</p>}
        <div className="row">
          <button className="btn primary" disabled={add.busy || !token} onClick={() => add.run(async () => { await api.addBot(token); setToken(''); bots.reload() })}>Save</button>
        </div>
      </div>
    </div>
  )
}
