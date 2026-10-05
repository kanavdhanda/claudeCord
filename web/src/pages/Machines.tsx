import { api } from '../api'
import { ago, useAction, useLoad } from '../hooks'

/** The machines this account has approved. Removing one cancels its token: it can no longer connect. */
export function Machines() {
  const machines = useLoad(api.machines)
  const state = useLoad(api.state, 4000)
  const act = useAction()
  const live = new Map(state.data?.devices.map((d) => [d.node, d]))
  return (
    <div className="stack">
      <h1>Machines</h1>
      <p className="muted">
        To add one, run <code>npx claudecord</code> (or <code>claudecord login</code>) on it and approve the code it shows.
      </p>
      <div className="card stack">
        {machines.data?.length === 0 && <p className="muted">No machines yet.</p>}
        {machines.data?.map((m) => (
          <div key={m.node} className="item">
            <div>
              <b>{m.node}</b>{' '}
              <span className={`pill ${live.get(m.node)?.connected ? 'ok' : 'off'}`}>{live.get(m.node)?.connected ? 'connected' : 'away'}</span>
              <div className="muted small">Approved {ago(m.since)}</div>
            </div>
            <button
              className="btn danger"
              disabled={act.busy}
              onClick={() => confirm(`Disconnect ${m.node} for good? It must be approved again to return.`) && act.run(async () => { await api.revokeMachine(m.node); machines.reload() })}
            >
              Remove
            </button>
          </div>
        ))}
      </div>
    </div>
  )
}
