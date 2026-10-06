import { createContext, useContext } from 'react'
import { Navigate, NavLink, Route, Routes, useLocation } from 'react-router-dom'
import { api, ApiError, type Me } from './api'
import { useLoad } from './hooks'
import { Activate } from './pages/Activate'
import { Pick } from './pages/Pick'
import { Bots } from './pages/Bots'
import { Insights } from './pages/Insights'
import { Login } from './pages/Login'
import { Machines } from './pages/Machines'
import { Overview } from './pages/Overview'
import { Setup } from './pages/Setup'

const MeContext = createContext<Me | null>(null)
export const useMe = () => useContext(MeContext)!

/** Everything except the sign-in page needs a signed-in person; anyone else is sent to sign in and brought back afterwards. */
export function App() {
  const { data: me, error } = useLoad(api.me)
  const loc = useLocation()
  if (loc.pathname === '/login') return <Login />
  if (error instanceof ApiError && error.status === 401) {
    const next = encodeURIComponent(loc.pathname + loc.search)
    return <Navigate to={`/login?next=${next}`} replace />
  }
  if (!me) return <div className="center muted">{error ? `Cannot reach the hub: ${error.message}` : 'Loading…'}</div>
  return (
    <MeContext.Provider value={me}>
      <div className="shell">
        <aside className="side">
          <div className="brand">
            <span className="logo" aria-hidden />
            claudeCord
          </div>
          <nav>
            <NavLink to="/" end>Overview</NavLink>
            <NavLink to="/insights">Insights</NavLink>
            <NavLink to="/setup">Add a project</NavLink>
            <NavLink to="/bots">Discord bots</NavLink>
            <NavLink to="/machines">Machines</NavLink>
          </nav>
          <div className="who">
            <span>{me.name}</span>
            <button
              className="link"
              onClick={async () => {
                await api.logout()
                location.href = '/login'
              }}
            >
              Sign out
            </button>
          </div>
        </aside>
        <main className="main">
          <Routes>
            <Route path="/" element={<Overview />} />
            <Route path="/insights" element={<Insights />} />
            <Route path="/setup" element={<Setup />} />
            <Route path="/bots" element={<Bots />} />
            <Route path="/machines" element={<Machines />} />
            <Route path="/activate" element={<Activate />} />
            <Route path="/pick" element={<Pick />} />
            <Route path="*" element={<Navigate to="/" replace />} />
          </Routes>
        </main>
      </div>
    </MeContext.Provider>
  )
}
