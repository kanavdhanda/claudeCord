import { useEffect, useState } from 'react'
import { Link, useSearchParams } from 'react-router-dom'
import { api } from '../api'
import { useAction } from '../hooks'

/** The page a machine's first run opens: it shows the code, asks which name this machine should have, and approves or refuses it. */
export function Activate() {
  const [q] = useSearchParams()
  const [code, setCode] = useState(q.get('code') ?? '')
  const [found, setFound] = useState<{ code: string; node: string } | null>(null)
  const [node, setNode] = useState('')
  const [done, setDone] = useState<'approved' | 'refused' | null>(null)
  const look = useAction()
  const act = useAction()

  const lookup = (c: string) =>
    look.run(async () => {
      const r = await api.lookupCode(c)
      setFound(r)
      setNode(r.node || 'my-machine')
    })

  useEffect(() => {
    if (q.get('code')) lookup(q.get('code')!)
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [])

  if (done)
    return (
      <div className="card narrow stack">
        <h2>{done === 'approved' ? 'Machine connected' : 'Machine refused'}</h2>
        <p className="muted">
          {done === 'approved'
            ? 'Go back to your terminal. It will pick this up in a few seconds.'
            : 'That machine will not be connected.'}
        </p>
        <Link className="btn primary" to="/">Continue to the dashboard</Link>
      </div>
    )

  return (
    <div className="card narrow stack">
      <h2>Connect a machine</h2>
      {!found ? (
        <>
          <p className="muted">Type the code your terminal showed.</p>
          <input className="code" value={code} placeholder="ABCD-EFGH" onChange={(e) => setCode(e.target.value)} />
          {look.error && <p className="error">{look.error}</p>}
          <button className="btn primary" disabled={look.busy || !code} onClick={() => lookup(code)}>Continue</button>
        </>
      ) : (
        <>
          <p>
            A machine is asking to join your account with the code <b className="mono">{found.code}</b>. Only approve it if you just
            started claudeCord on a machine of yours.
          </p>
          <label>
            Name this machine
            <input value={node} onChange={(e) => setNode(e.target.value)} />
          </label>
          {act.error && <p className="error">{act.error}</p>}
          <div className="row">
            <button
              className="btn primary"
              disabled={act.busy || !node}
              onClick={() =>
                act.run(async () => {
                  await api.approve(found.code, node)
                  setDone('approved')
                })
              }
            >
              Approve
            </button>
            <button
              className="btn"
              disabled={act.busy}
              onClick={() =>
                act.run(async () => {
                  await api.deny(found.code)
                  setDone('refused')
                })
              }
            >
              Refuse
            </button>
          </div>
        </>
      )}
    </div>
  )
}

