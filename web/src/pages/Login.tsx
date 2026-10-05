import { useSearchParams } from 'react-router-dom'

export function Login() {
  const [q] = useSearchParams()
  const next = q.get('next') ?? '/'
  return (
    <div className="center">
      <div className="card narrow stack">
        <div className="brand big">
          <span className="logo" aria-hidden />
          claudeCord
        </div>
        <p className="muted">Run coding agents on any machine and manage them from a Discord group chat.</p>
        <a className="btn primary" href={`/auth/login?next=${encodeURIComponent(next)}`}>
          Sign in with Discord
        </a>
        <p className="small muted">We only ask for your Discord id and name. Nothing else about you.</p>
      </div>
    </div>
  )
}
