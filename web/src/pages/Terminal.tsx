import { useEffect, useRef, useState } from 'react'
import { Link, useNavigate, useParams } from 'react-router-dom'
import { api, type TerminalSession } from '../api'

type TerminalTab = {
  windowId: string
  paneId: string
  index: number
  name: string
  isAgent: boolean
}

type Cell = {
  ch: string
  fg: string
  bg: string
  bold: boolean
  underline: boolean
  inverse: boolean
}

const DEFAULT_FG = '#ececf0'
const DEFAULT_BG = '#0d0e12'

const ANSI_16 = [
  '#1b1b22', '#e05263', '#42b883', '#e5c07b',
  '#61afef', '#c678dd', '#56b6c2', '#abb2bf',
  '#5c6370', '#ff6b81', '#55efc4', '#ffeaa7',
  '#74b9ff', '#fd79a8', '#81ecec', '#ffffff',
]

function color256(n: number): string {
  if (n < 16) return ANSI_16[n] ?? DEFAULT_FG
  if (n >= 232) {
    const g = Math.min(255, Math.max(0, 8 + (n - 232) * 10))
    const hex = g.toString(16).padStart(2, '0')
    return `#${hex}${hex}${hex}`
  }
  const idx = n - 16
  const r = Math.floor(idx / 36)
  const g = Math.floor((idx % 36) / 6)
  const b = idx % 6
  const conv = (v: number) => (v === 0 ? 0 : 55 + v * 40).toString(16).padStart(2, '0')
  return `#${conv(r)}${conv(g)}${conv(b)}`
}

function emptyCell(): Cell {
  return { ch: ' ', fg: DEFAULT_FG, bg: DEFAULT_BG, bold: false, underline: false, inverse: false }
}

class VtScreen {
  cols: number
  rows: number
  grid: Cell[][]
  altGrid: Cell[][] | null = null
  cx = 0
  cy = 0
  savedCx = 0
  savedCy = 0
  fg = DEFAULT_FG
  bg = DEFAULT_BG
  bold = false
  underline = false
  inverse = false
  cursorVisible = true
  private state: 'normal' | 'esc' | 'csi' | 'osc' = 'normal'
  private csiBuf = ''
  private oscEsc = false

  constructor(cols: number, rows: number) {
    this.cols = cols
    this.rows = rows
    this.grid = this.makeGrid(cols, rows)
  }

  private makeGrid(cols: number, rows: number): Cell[][] {
    const out: Cell[][] = []
    for (let r = 0; r < rows; r++) {
      const row: Cell[] = []
      for (let c = 0; c < cols; c++) row.push(emptyCell())
      out.push(row)
    }
    return out
  }

  resize(cols: number, rows: number) {
    if (cols === this.cols && rows === this.rows) return
    const next = this.makeGrid(cols, rows)
    const copyRows = Math.min(this.rows, rows)
    const copyCols = Math.min(this.cols, cols)
    for (let r = 0; r < copyRows; r++) {
      for (let c = 0; c < copyCols; c++) {
        next[r][c] = this.grid[r][c]
      }
    }
    this.cols = cols
    this.rows = rows
    this.grid = next
    this.cx = Math.min(this.cx, Math.max(0, cols - 1))
    this.cy = Math.min(this.cy, Math.max(0, rows - 1))
  }

  reset() {
    this.grid = this.makeGrid(this.cols, this.rows)
    this.altGrid = null
    this.cx = 0
    this.cy = 0
    this.fg = DEFAULT_FG
    this.bg = DEFAULT_BG
    this.bold = false
    this.underline = false
    this.inverse = false
    this.state = 'normal'
    this.csiBuf = ''
  }

  private scrollUp(lines = 1) {
    for (let i = 0; i < lines; i++) {
      this.grid.shift()
      const row: Cell[] = []
      for (let c = 0; c < this.cols; c++) row.push(emptyCell())
      this.grid.push(row)
    }
  }

  private putChar(ch: string) {
    if (this.cx >= this.cols) {
      this.cx = 0
      this.cy++
      if (this.cy >= this.rows) {
        this.scrollUp(1)
        this.cy = this.rows - 1
      }
    }
    if (this.cy >= 0 && this.cy < this.rows && this.cx >= 0 && this.cx < this.cols) {
      this.grid[this.cy][this.cx] = {
        ch,
        fg: this.fg,
        bg: this.bg,
        bold: this.bold,
        underline: this.underline,
        inverse: this.inverse,
      }
    }
    this.cx++
  }

  write(text: string) {
    for (let i = 0; i < text.length; i++) {
      const ch = text[i]
      const code = ch.charCodeAt(0)
      if (this.state === 'osc') {
        if (code === 0x07) {
          this.state = 'normal'
        } else if (this.oscEsc && ch === '\\') {
          this.oscEsc = false
          this.state = 'normal'
        } else {
          this.oscEsc = code === 0x1b
        }
        continue
      }
      if (this.state === 'esc') {
        if (ch === '[') {
          this.state = 'csi'
          this.csiBuf = ''
        } else if (ch === ']') {
          this.state = 'osc'
          this.oscEsc = false
        } else if (ch === '7') {
          this.savedCx = this.cx
          this.savedCy = this.cy
          this.state = 'normal'
        } else if (ch === '8') {
          this.cx = this.savedCx
          this.cy = this.savedCy
          this.state = 'normal'
        } else if (ch === 'c') {
          this.reset()
        } else {
          this.state = 'normal'
        }
        continue
      }
      if (this.state === 'csi') {
        if (code >= 0x40 && code <= 0x7e) {
          this.handleCsi(this.csiBuf, ch)
          this.state = 'normal'
          this.csiBuf = ''
        } else {
          this.csiBuf += ch
        }
        continue
      }
      if (code === 0x1b) {
        this.state = 'esc'
      } else if (ch === '\r') {
        this.cx = 0
      } else if (ch === '\n') {
        this.cy++
        if (this.cy >= this.rows) {
          this.scrollUp(1)
          this.cy = this.rows - 1
        }
      } else if (ch === '\b') {
        if (this.cx > 0) this.cx--
      } else if (ch === '\t') {
        const nextTab = (Math.floor(this.cx / 8) + 1) * 8
        while (this.cx < Math.min(nextTab, this.cols)) this.putChar(' ')
      } else if (code >= 0x20) {
        this.putChar(ch)
      }
    }
  }

  private handleCsi(raw: string, finalCh: string) {
    const priv = raw.startsWith('?')
    const clean = priv ? raw.slice(1) : raw
    const parts = clean.length ? clean.split(';').map((s) => parseInt(s || '0', 10)) : []
    const p0 = parts[0] || 0
    const p1 = parts[1] || 0

    if (priv) {
      if (finalCh === 'h') {
        if (p0 === 25) this.cursorVisible = true
        if (p0 === 1049 || p0 === 47) {
          this.savedCx = this.cx
          this.savedCy = this.cy
          this.altGrid = this.grid
          this.grid = this.makeGrid(this.cols, this.rows)
          this.cx = 0
          this.cy = 0
        }
      } else if (finalCh === 'l') {
        if (p0 === 25) this.cursorVisible = false
        if ((p0 === 1049 || p0 === 47) && this.altGrid) {
          this.grid = this.altGrid
          this.altGrid = null
          this.cx = Math.min(this.savedCx, this.cols - 1)
          this.cy = Math.min(this.savedCy, this.rows - 1)
        }
      }
      return
    }

    switch (finalCh) {
      case 'A':
        this.cy = Math.max(0, this.cy - Math.max(1, p0))
        break
      case 'B':
        this.cy = Math.min(this.rows - 1, this.cy + Math.max(1, p0))
        break
      case 'C':
        this.cx = Math.min(this.cols - 1, this.cx + Math.max(1, p0))
        break
      case 'D':
        this.cx = Math.max(0, this.cx - Math.max(1, p0))
        break
      case 'G':
        this.cx = Math.min(this.cols - 1, Math.max(0, (p0 || 1) - 1))
        break
      case 'd':
        this.cy = Math.min(this.rows - 1, Math.max(0, (p0 || 1) - 1))
        break
      case 'H':
      case 'f':
        this.cy = Math.min(this.rows - 1, Math.max(0, (p0 || 1) - 1))
        this.cx = Math.min(this.cols - 1, Math.max(0, (p1 || 1) - 1))
        break
      case 'J':
        if (p0 === 2 || p0 === 3) {
          for (let r = 0; r < this.rows; r++) {
            for (let c = 0; c < this.cols; c++) this.grid[r][c] = emptyCell()
          }
        } else if (p0 === 0) {
          for (let c = this.cx; c < this.cols; c++) this.grid[this.cy][c] = emptyCell()
          for (let r = this.cy + 1; r < this.rows; r++) {
            for (let c = 0; c < this.cols; c++) this.grid[r][c] = emptyCell()
          }
        } else if (p0 === 1) {
          for (let r = 0; r < this.cy; r++) {
            for (let c = 0; c < this.cols; c++) this.grid[r][c] = emptyCell()
          }
          for (let c = 0; c <= Math.min(this.cx, this.cols - 1); c++) this.grid[this.cy][c] = emptyCell()
        }
        break
      case 'K':
        if (p0 === 0) {
          for (let c = this.cx; c < this.cols; c++) this.grid[this.cy][c] = emptyCell()
        } else if (p0 === 1) {
          for (let c = 0; c <= Math.min(this.cx, this.cols - 1); c++) this.grid[this.cy][c] = emptyCell()
        } else if (p0 === 2) {
          for (let c = 0; c < this.cols; c++) this.grid[this.cy][c] = emptyCell()
        }
        break
      case 'S':
        this.scrollUp(Math.max(1, p0))
        break
      case 'm':
        this.handleSgr(parts.length ? parts : [0])
        break
    }
  }

  private handleSgr(codes: number[]) {
    for (let i = 0; i < codes.length; i++) {
      const c = codes[i]
      if (c === 0) {
        this.fg = DEFAULT_FG
        this.bg = DEFAULT_BG
        this.bold = false
        this.underline = false
        this.inverse = false
      } else if (c === 1) {
        this.bold = true
      } else if (c === 22) {
        this.bold = false
      } else if (c === 4) {
        this.underline = true
      } else if (c === 24) {
        this.underline = false
      } else if (c === 7) {
        this.inverse = true
      } else if (c === 27) {
        this.inverse = false
      } else if (c >= 30 && c <= 37) {
        this.fg = ANSI_16[c - 30 + (this.bold ? 8 : 0)]
      } else if (c === 39) {
        this.fg = DEFAULT_FG
      } else if (c >= 40 && c <= 47) {
        this.bg = ANSI_16[c - 40]
      } else if (c === 49) {
        this.bg = DEFAULT_BG
      } else if (c >= 90 && c <= 97) {
        this.fg = ANSI_16[c - 90 + 8]
      } else if (c >= 100 && c <= 107) {
        this.bg = ANSI_16[c - 100 + 8]
      } else if (c === 38 || c === 48) {
        const isFg = c === 38
        const mode = codes[i + 1]
        if (mode === 5 && i + 2 < codes.length) {
          const col = color256(codes[i + 2])
          if (isFg) this.fg = col
          else this.bg = col
          i += 2
        } else if (mode === 2 && i + 4 < codes.length) {
          const r = Math.min(255, Math.max(0, codes[i + 2])).toString(16).padStart(2, '0')
          const g = Math.min(255, Math.max(0, codes[i + 3])).toString(16).padStart(2, '0')
          const b = Math.min(255, Math.max(0, codes[i + 4])).toString(16).padStart(2, '0')
          const col = `#${r}${g}${b}`
          if (isFg) this.fg = col
          else this.bg = col
          i += 4
        }
      }
    }
  }

  plainText(): string {
    return this.grid
      .map((row) => row.map((c) => c.ch).join('').replace(/\s+$/, ''))
      .join('\n')
      .replace(/\n+$/, '')
  }

  renderToCanvas(canvas: HTMLCanvasElement, cellW: number, cellH: number) {
    const dpr = window.devicePixelRatio || 1
    const w = Math.floor(this.cols * cellW)
    const h = Math.floor(this.rows * cellH)
    if (canvas.width !== Math.floor(w * dpr) || canvas.height !== Math.floor(h * dpr)) {
      canvas.width = Math.floor(w * dpr)
      canvas.height = Math.floor(h * dpr)
    }
    const ctx = canvas.getContext('2d')
    if (!ctx) return
    ctx.save()
    ctx.scale(dpr, dpr)
    ctx.fillStyle = DEFAULT_BG
    ctx.fillRect(0, 0, w, h)
    ctx.textBaseline = 'top'
    const padY = 2

    for (let r = 0; r < this.rows; r++) {
      const y = r * cellH
      const row = this.grid[r]
      for (let c = 0; c < this.cols; c++) {
        const cell = row[c]
        const x = c * cellW
        const fg = cell.inverse ? cell.bg : cell.fg
        const bg = cell.inverse ? cell.fg : cell.bg
        if (bg !== DEFAULT_BG) {
          ctx.fillStyle = bg
          ctx.fillRect(x, y, Math.ceil(cellW), Math.ceil(cellH))
        }
        if (cell.ch !== ' ') {
          ctx.font = `${cell.bold ? 'bold ' : ''}13px ui-monospace, Menlo, Consolas, monospace`
          ctx.fillStyle = fg
          ctx.fillText(cell.ch, x, y + padY)
        }
        if (cell.underline) {
          ctx.fillStyle = fg
          ctx.fillRect(x, y + cellH - 2, cellW, 1)
        }
      }
    }

    if (this.cursorVisible && this.cy >= 0 && this.cy < this.rows && this.cx >= 0 && this.cx < this.cols) {
      ctx.fillStyle = 'rgba(116, 128, 255, 0.65)'
      ctx.fillRect(this.cx * cellW, this.cy * cellH, Math.ceil(cellW), Math.ceil(cellH))
    }
    ctx.restore()
  }
}

function b64UrlEncode(bytes: Uint8Array): string {
  let bin = ''
  for (let i = 0; i < bytes.length; i++) bin += String.fromCharCode(bytes[i])
  return btoa(bin).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '')
}

function b64UrlDecode(str: string): Uint8Array {
  const pad = str.length % 4 === 0 ? '' : '='.repeat(4 - (str.length % 4))
  const b64 = str.replace(/-/g, '+').replace(/_/g, '/') + pad
  const bin = atob(b64)
  const out = new Uint8Array(bin.length)
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i)
  return out
}

async function importE2eKey(b64Key: string): Promise<CryptoKey | null> {
  if (!b64Key || !window.crypto?.subtle) return null
  try {
    const raw = b64UrlDecode(b64Key)
    return await window.crypto.subtle.importKey('raw', raw as BufferSource, { name: 'AES-GCM' }, false, ['encrypt', 'decrypt'])
  } catch {
    return null
  }
}

async function sealEnvelope(env: unknown, key: CryptoKey | null, sessionId: string, generation: number): Promise<string> {
  const json = JSON.stringify(env)
  if (!key || !window.crypto?.subtle) return json
  const nonce = new Uint8Array(12)
  window.crypto.getRandomValues(nonce)
  const aad = new TextEncoder().encode(`claudecord-term-e2e:${sessionId}:${generation}`)
  const plain = new TextEncoder().encode(json)
  const ctBuf = await window.crypto.subtle.encrypt(
    { name: 'AES-GCM', iv: nonce as BufferSource, additionalData: aad as BufferSource },
    key,
    plain as BufferSource,
  )
  const ct = new Uint8Array(ctBuf)
  const combined = new Uint8Array(nonce.length + ct.length)
  combined.set(nonce, 0)
  combined.set(ct, nonce.length)
  return `e2e1.${b64UrlEncode(combined)}`
}

async function openEnvelope(raw: string, key: CryptoKey | null, sessionId: string, generation: number): Promise<any | null> {
  try {
    if (raw.startsWith('e2e1.')) {
      if (!key || !window.crypto?.subtle) return null
      const combined = b64UrlDecode(raw.slice(5))
      if (combined.length < 28) return null
      const nonce = combined.slice(0, 12)
      const ct = combined.slice(12)
      const aad = new TextEncoder().encode(`claudecord-term-e2e:${sessionId}:${generation}`)
      const plainBuf = await window.crypto.subtle.decrypt(
        { name: 'AES-GCM', iv: nonce as BufferSource, additionalData: aad as BufferSource },
        key,
        ct as BufferSource,
      )
      return JSON.parse(new TextDecoder().decode(plainBuf))
    }
    return JSON.parse(raw)
  } catch {
    return null
  }
}

function statusBadgeLabel(state: string, transport: string): { label: string; cls: string } {
  if (state === 'revoked') return { label: 'Revoked', cls: 'warn' }
  if (state === 'failed') return { label: 'Failed', cls: 'off' }
  if (state === 'reconnecting') return { label: 'Reconnecting', cls: 'warn' }
  if (state === 'connecting') return { label: 'Connecting', cls: 'busy' }
  if (transport === 'webrtc_direct' || transport === 'webrtc') return { label: 'Connected directly (WebRTC)', cls: 'ok' }
  if (transport === 'webrtc_turn_udp') return { label: 'Connected via TURN/UDP', cls: 'ok' }
  if (transport === 'webrtc_turn_tcp') return { label: 'Connected via TURN/TCP', cls: 'ok' }
  if (transport === 'wss' || transport === 'wss_fallback') return { label: 'Connected via WSS fallback', cls: 'ok' }
  return { label: 'Connecting', cls: 'busy' }
}

/** Interactive browser & mobile Safari terminal attached to an existing claudeCord-managed agent tmux session. */
export function TerminalPage() {
  const { project = '', agent = '' } = useParams<{ project: string; agent: string }>()
  const navigate = useNavigate()
  const canvasRef = useRef<HTMLCanvasElement | null>(null)
  const boxRef = useRef<HTMLDivElement | null>(null)
  const screensRef = useRef<Map<string, VtScreen>>(new Map())
  const activePaneRef = useRef<string>('%0')
  const seqOutRef = useRef<number>(1)
  const sessionRef = useRef<TerminalSession | null>(null)
  const e2eKeyRef = useRef<CryptoKey | null>(null)
  const wsRef = useRef<WebSocket | null>(null)
  const dcRef = useRef<RTCDataChannel | null>(null)
  const pcRef = useRef<RTCPeerConnection | null>(null)
  const dimsRef = useRef<{ cols: number; rows: number; cellW: number; cellH: number }>({
    cols: 100,
    rows: 30,
    cellW: 7.85,
    cellH: 18,
  })

  const [connState, setConnState] = useState<string>('connecting')
  const [transport, setTransport] = useState<string>('connecting')
  const [rttMs, setRttMs] = useState<number | null>(null)
  const [bytesTx, setBytesTx] = useState<number>(0)
  const [bytesRx, setBytesRx] = useState<number>(0)
  const [generation, setGeneration] = useState<number>(1)
  const [tabs, setTabs] = useState<TerminalTab[]>([
    { windowId: '@0', paneId: '%0', index: 0, name: agent || 'agent', isAgent: true },
  ])
  const [activePane, setActivePane] = useState<string>('%0')
  const [mirrorText, setMirrorText] = useState<string>('')
  const [showMirror, setShowMirror] = useState<boolean>(false)
  const [showDiag, setShowDiag] = useState<boolean>(false)
  const [ctrlSticky, setCtrlSticky] = useState<boolean>(false)
  const [altSticky, setAltSticky] = useState<boolean>(false)
  const [mobileLine, setMobileLine] = useState<string>('')
  const [errBanner, setErrBanner] = useState<string | null>(null)
  const [reconnectTick, setReconnectTick] = useState<number>(0)

  const getScreen = (paneId: string): VtScreen => {
    let scr = screensRef.current.get(paneId)
    if (!scr) {
      scr = new VtScreen(dimsRef.current.cols, dimsRef.current.rows)
      screensRef.current.set(paneId, scr)
    }
    return scr
  }

  const redrawActive = () => {
    const scr = getScreen(activePaneRef.current)
    if (canvasRef.current) {
      scr.renderToCanvas(canvasRef.current, dimsRef.current.cellW, dimsRef.current.cellH)
    }
    setMirrorText(scr.plainText())
  }

  const sendMessage = async (msg: Record<string, unknown>) => {
    const sess = sessionRef.current
    if (!sess) return
    const env = {
      v: 1,
      generation: sess.generation,
      seq: seqOutRef.current++,
      msg,
    }
    const dc = dcRef.current
    if (dc && dc.readyState === 'open') {
      const raw = JSON.stringify(env)
      dc.send(raw)
      setBytesTx((b) => b + raw.length)
      return
    }
    const ws = wsRef.current
    if (ws && ws.readyState === WebSocket.OPEN) {
      const payload = await sealEnvelope(env, e2eKeyRef.current, sess.sessionId, sess.generation)
      ws.send(JSON.stringify({ type: 'term.frame', generation: sess.generation, payload }))
      setBytesTx((b) => b + payload.length)
    }
  }

  const sendInputBytes = (bytes: Uint8Array) => {
    if (bytes.length === 0) return
    void sendMessage({
      op: 'input',
      pane: activePaneRef.current,
      data: b64UrlEncode(bytes),
    })
  }

  const sendInputText = (text: string) => {
    if (!text) return
    let out = text
    if (ctrlSticky && out.length === 1) {
      const ch = out.toUpperCase().charCodeAt(0)
      if (ch >= 64 && ch <= 95) {
        out = String.fromCharCode(ch - 64)
      }
      setCtrlSticky(false)
    }
    if (altSticky) {
      out = `\x1b${out}`
      setAltSticky(false)
    }
    sendInputBytes(new TextEncoder().encode(out))
  }

  const handleIncomingEnvelope = (env: any) => {
    if (!env || !env.msg) return
    const msg = env.msg
    switch (msg.op) {
      case 'output': {
        const pane = msg.pane || '%0'
        const scr = getScreen(pane)
        if (msg.snapshot) scr.reset()
        const rawBytes = b64UrlDecode(msg.data || '')
        const decoded = new TextDecoder().decode(rawBytes)
        scr.write(decoded)
        if (pane === activePaneRef.current) {
          redrawActive()
        }
        break
      }
      case 'tabs': {
        if (Array.isArray(msg.tabs) && msg.tabs.length > 0) {
          setTabs(msg.tabs)
        }
        if (typeof msg.activePane === 'string' && msg.activePane) {
          activePaneRef.current = msg.activePane
          setActivePane(msg.activePane)
          redrawActive()
        }
        break
      }
      case 'pong': {
        if (typeof msg.ts === 'number') {
          setRttMs(Math.max(0, Date.now() - msg.ts))
        }
        break
      }
      case 'error': {
        setErrBanner(`${msg.code}: ${msg.message}`)
        break
      }
      case 'close': {
        setConnState('failed')
        setErrBanner(`Session closed: ${msg.reason || 'detached'}`)
        break
      }
    }
  }

  useEffect(() => {
    const measureAndFit = () => {
      const box = boxRef.current
      if (!box) return
      const probe = document.createElement('canvas').getContext('2d')
      let cw = 7.85
      if (probe) {
        probe.font = '13px ui-monospace, Menlo, Consolas, monospace'
        cw = Math.max(6, probe.measureText('M').width)
      }
      const ch = 18
      const availW = Math.max(240, box.clientWidth - 16)
      const vh = window.visualViewport ? window.visualViewport.height : window.innerHeight
      const availH = Math.max(180, Math.min(box.clientHeight || vh - 240, vh - 210))
      const cols = Math.min(300, Math.max(30, Math.floor(availW / cw)))
      const rows = Math.min(120, Math.max(10, Math.floor(availH / ch)))
      const prev = dimsRef.current
      dimsRef.current = { cols, rows, cellW: cw, cellH: ch }
      screensRef.current.forEach((s) => s.resize(cols, rows))
      redrawActive()
      if (prev.cols !== cols || prev.rows !== rows) {
        void sendMessage({ op: 'resize', pane: activePaneRef.current, cols, rows })
      }
    }

    measureAndFit()
    const ro = typeof ResizeObserver !== 'undefined' ? new ResizeObserver(measureAndFit) : null
    if (ro && boxRef.current) ro.observe(boxRef.current)
    window.addEventListener('resize', measureAndFit)
    window.visualViewport?.addEventListener('resize', measureAndFit)
    return () => {
      ro?.disconnect()
      window.removeEventListener('resize', measureAndFit)
      window.visualViewport?.removeEventListener('resize', measureAndFit)
    }
  }, [])

  useEffect(() => {
    if (!project || !agent) return
    let cancelled = false
    let pingTimer: number | null = null
    let fallbackTimer: number | null = null

    const start = async () => {
      setConnState('connecting')
      setTransport('connecting')
      setErrBanner(null)
      seqOutRef.current = 1

      let pc: RTCPeerConnection | null = null
      let offerSdp: string | undefined
      try {
        if (typeof RTCPeerConnection !== 'undefined') {
          pc = new RTCPeerConnection()
          pcRef.current = pc
          const dc = pc.createDataChannel('terminal', { ordered: true })
          dcRef.current = dc
          dc.onopen = () => {
            if (cancelled) return
            if (fallbackTimer) window.clearTimeout(fallbackTimer)
            setConnState('connected')
            setTransport((prev) => (prev.startsWith('webrtc') ? prev : 'webrtc_direct'))
            void sendMessage({ op: 'sync', pane: activePaneRef.current })
          }
          dc.onmessage = async (ev) => {
            if (cancelled) return
            const text = typeof ev.data === 'string' ? ev.data : new TextDecoder().decode(ev.data)
            setBytesRx((b) => b + text.length)
            const env = await openEnvelope(text, e2eKeyRef.current, sessionRef.current?.sessionId || '', sessionRef.current?.generation || 1)
            if (env) handleIncomingEnvelope(env)
          }
          const offer = await pc.createOffer()
          await pc.setLocalDescription(offer)
          offerSdp = offer.sdp
        }
      } catch {
        pc = null
      }

      try {
        const { cols, rows } = dimsRef.current
        const sess = await api.openTerminal(project, agent, cols, rows, offerSdp)
        if (cancelled) return
        sessionRef.current = sess
        setGeneration(sess.generation)
        e2eKeyRef.current = await importE2eKey(sess.e2eKey)

        if (pc) {
          pc.onicecandidate = (ev) => {
            if (ev.candidate && ev.candidate.candidate && sessionRef.current) {
              void api.signalTerminal(sessionRef.current.sessionId, { candidate: ev.candidate.candidate })
            }
          }
        }

        const proto = location.protocol === 'https:' ? 'wss:' : 'ws:'
        const wsUrl = `${proto}//${location.host}${sess.wsPath}?session_id=${encodeURIComponent(sess.sessionId)}&grant=${encodeURIComponent(sess.grant)}`
        const ws = new WebSocket(wsUrl)
        wsRef.current = ws

        ws.onopen = () => {
          if (cancelled) return
          fallbackTimer = window.setTimeout(() => {
            if (cancelled) return
            if (!dcRef.current || dcRef.current.readyState !== 'open') {
              setConnState('connected')
              setTransport('wss_fallback')
              void sendMessage({ op: 'sync', pane: activePaneRef.current })
            }
          }, 600)
        }

        ws.onmessage = async (ev) => {
          if (cancelled || typeof ev.data !== 'string') return
          let frame: any
          try {
            frame = JSON.parse(ev.data)
          } catch {
            return
          }
          const t = frame.t || frame.type
          if (t === 'term.ready') {
            if (frame.answerSdp && pc && pc.signalingState !== 'stable') {
              try {
                await pc.setRemoteDescription({ type: 'answer', sdp: frame.answerSdp })
                if (Array.isArray(frame.candidates)) {
                  for (const c of frame.candidates) {
                    await pc.addIceCandidate({ candidate: c, sdpMid: '0', sdpMLineIndex: 0 }).catch(() => {})
                  }
                }
              } catch {
                // Automatic fallback to WSS relay remains active
              }
            }
            if (frame.transport === 'wss' || !frame.answerSdp) {
              setConnState('connected')
              setTransport('wss_fallback')
              void sendMessage({ op: 'sync', pane: activePaneRef.current })
            }
          } else if (t === 'term.state') {
            if (frame.state) setConnState(frame.state)
            if (frame.transport) setTransport(frame.transport)
            if (typeof frame.rttMs === 'number') setRttMs(frame.rttMs)
            if (typeof frame.bytesTx === 'number') setBytesRx(frame.bytesTx)
            if (typeof frame.bytesRx === 'number') setBytesTx(frame.bytesRx)
            if (frame.state === 'failed' || frame.state === 'revoked') {
              setErrBanner(frame.detail || `Terminal ${frame.state}`)
            }
          } else if (t === 'term.frame' && typeof frame.payload === 'string') {
            setBytesRx((b) => b + frame.payload.length)
            const env = await openEnvelope(frame.payload, e2eKeyRef.current, sess.sessionId, sess.generation)
            if (env) {
              setConnState('connected')
              setTransport((prev) => (prev.startsWith('webrtc') ? prev : 'wss_fallback'))
              handleIncomingEnvelope(env)
            }
          }
        }

        ws.onclose = (ev) => {
          if (cancelled) return
          if (ev.code === 4003) {
            setConnState('revoked')
            setErrBanner('Terminal session grant was revoked or expired.')
          } else if (!dcRef.current || dcRef.current.readyState !== 'open') {
            setConnState('reconnecting')
          }
        }

        pingTimer = window.setInterval(() => {
          void sendMessage({ op: 'ping', ts: Date.now() })
        }, 5000)
      } catch (e: any) {
        if (!cancelled) {
          setConnState('failed')
          setErrBanner(e?.message || 'Failed to open terminal session')
        }
      }
    }

    void start()

    return () => {
      cancelled = true
      if (pingTimer) window.clearInterval(pingTimer)
      if (fallbackTimer) window.clearTimeout(fallbackTimer)
      dcRef.current?.close()
      pcRef.current?.close()
      wsRef.current?.close()
    }
  }, [project, agent, reconnectTick])

  const onCanvasKeyDown = (e: React.KeyboardEvent<HTMLDivElement>) => {
    if (e.metaKey && (e.key === 'c' || e.key === 'v' || e.key === 'r')) return
    e.preventDefault()
    if (e.ctrlKey && e.key.length === 1) {
      const code = e.key.toUpperCase().charCodeAt(0)
      if (code >= 64 && code <= 95) {
        sendInputBytes(new Uint8Array([code - 64]))
        return
      }
    }
    switch (e.key) {
      case 'Enter':
        sendInputText('\r')
        break
      case 'Backspace':
        sendInputText('\x7f')
        break
      case 'Tab':
        sendInputText(e.shiftKey ? '\x1b[Z' : '\t')
        break
      case 'Escape':
        sendInputText('\x1b')
        break
      case 'ArrowUp':
        sendInputText('\x1b[A')
        break
      case 'ArrowDown':
        sendInputText('\x1b[B')
        break
      case 'ArrowRight':
        sendInputText('\x1b[C')
        break
      case 'ArrowLeft':
        sendInputText('\x1b[D')
        break
      case 'Home':
        sendInputText('\x1b[H')
        break
      case 'End':
        sendInputText('\x1b[F')
        break
      case 'PageUp':
        sendInputText('\x1b[5~')
        break
      case 'PageDown':
        sendInputText('\x1b[6~')
        break
      case 'Delete':
        sendInputText('\x1b[3~')
        break
      default:
        if (e.key.length === 1) {
          sendInputText(e.altKey ? `\x1b${e.key}` : e.key)
        }
        break
    }
  }

  const selectTab = (paneId: string) => {
    activePaneRef.current = paneId
    setActivePane(paneId)
    redrawActive()
    void sendMessage({ op: 'tab_select', pane: paneId })
  }

  const openNewTab = () => {
    void sendMessage({ op: 'tab_open' })
  }

  const closeTab = (paneId: string, e: React.MouseEvent) => {
    e.stopPropagation()
    void sendMessage({ op: 'tab_close', pane: paneId })
  }

  const revokeAndClose = async () => {
    const sess = sessionRef.current
    if (sess) {
      await api.revokeTerminal(sess.sessionId, 'user closed terminal').catch(() => {})
    }
    navigate('/')
  }

  const badge = statusBadgeLabel(connState, transport)

  return (
    <div className="stack term-page">
      <div className="row between">
        <div className="row">
          <Link className="btn" to="/">← Overview</Link>
          <h2>{project} / {agent}</h2>
          <span className={`pill ${badge.cls}`} data-testid="term-status-badge">{badge.label}</span>
          {rttMs !== null && <span className="muted small">{rttMs} ms</span>}
        </div>
        <div className="row">
          <button className="btn" onClick={() => setShowMirror((v) => !v)}>
            {showMirror ? 'Canvas view' : 'Copyable text'}
          </button>
          <button className="btn" onClick={() => setShowDiag((v) => !v)}>
            Diagnostics
          </button>
          <button className="btn" onClick={() => setReconnectTick((n) => n + 1)}>
            Reconnect
          </button>
          <button className="btn danger" onClick={() => void revokeAndClose()}>
            Revoke &amp; Exit
          </button>
        </div>
      </div>

      {errBanner && (
        <div className="card warn">
          <div className="row between">
            <span>{errBanner}</span>
            <button className="btn" onClick={() => setReconnectTick((n) => n + 1)}>Retry</button>
          </div>
        </div>
      )}

      {showDiag && (
        <div className="card tight mono small" data-testid="term-diagnostics">
          <div className="row between">
            <span>Session: {sessionRef.current?.sessionId ?? '—'} (gen {generation})</span>
            <span>Transport: {transport} · E2E: {e2eKeyRef.current ? 'AES-256-GCM (e2e1)' : 'Hub TLS'}</span>
            <span>Grid: {dimsRef.current.cols}×{dimsRef.current.rows} · TX {bytesTx}B / RX {bytesRx}B</span>
          </div>
        </div>
      )}

      <div className="term-tabs" role="tablist" aria-label="tmux session windows">
        {tabs.map((t) => (
          <div
            key={t.paneId}
            role="tab"
            aria-selected={t.paneId === activePane}
            className={`term-tab ${t.paneId === activePane ? 'on' : ''}`}
            onClick={() => selectTab(t.paneId)}
          >
            <span>{t.isAgent ? `🤖 ${t.name} (${t.index})` : `💻 ${t.name} (${t.index})`}</span>
            {!t.isAgent && (
              <button
                type="button"
                className="term-tab-close"
                aria-label={`Close tab ${t.name}`}
                onClick={(e) => closeTab(t.paneId, e)}
              >
                ×
              </button>
            )}
          </div>
        ))}
        <button type="button" className="btn term-tab-add" onClick={openNewTab} title="Open a shell tab alongside the agent">
          + Tab
        </button>
      </div>

      <div
        ref={boxRef}
        className="term-viewport"
        tabIndex={0}
        onKeyDown={onCanvasKeyDown}
        onPaste={(e) => {
          const pasted = e.clipboardData.getData('text')
          if (pasted) {
            e.preventDefault()
            sendInputText(pasted)
          }
        }}
        aria-label="Interactive terminal viewport"
      >
        <canvas ref={canvasRef} className={showMirror ? 'term-hidden' : 'term-canvas'} />
        {showMirror && (
          <pre className="term-mirror">{mirrorText || 'Terminal output will appear here…'}</pre>
        )}
      </div>

      <div className="term-touchbar" role="toolbar" aria-label="Mobile and shortcut terminal keys">
        <button type="button" className="btn" onClick={() => sendInputText('\x1b')}>Esc</button>
        <button type="button" className="btn" onClick={() => sendInputText('\t')}>Tab</button>
        <button type="button" className="btn" onClick={() => sendInputText('\x1b[Z')}>Shift+Tab</button>
        <button
          type="button"
          className={`btn ${ctrlSticky ? 'primary' : ''}`}
          onClick={() => setCtrlSticky((v) => !v)}
        >
          Ctrl
        </button>
        <button
          type="button"
          className={`btn ${altSticky ? 'primary' : ''}`}
          onClick={() => setAltSticky((v) => !v)}
        >
          Alt
        </button>
        <button type="button" className="btn" onClick={() => sendInputBytes(new Uint8Array([3]))}>Ctrl+C</button>
        <button type="button" className="btn" onClick={() => sendInputBytes(new Uint8Array([4]))} title="Send EOF (closes secondary shell tab)">Ctrl+D</button>
        <button type="button" className="btn" onClick={() => sendInputBytes(new Uint8Array([12]))}>Ctrl+L</button>
        <button type="button" className="btn" onClick={() => sendInputText('\x1b[A')}>↑</button>
        <button type="button" className="btn" onClick={() => sendInputText('\x1b[B')}>↓</button>
        <button type="button" className="btn" onClick={() => sendInputText('\x1b[D')}>←</button>
        <button type="button" className="btn" onClick={() => sendInputText('\x1b[C')}>→</button>
        <button type="button" className="btn" onClick={() => sendInputText('\r')}>Enter</button>
      </div>

      <form
        className="term-mobile-input"
        onSubmit={(e) => {
          e.preventDefault()
          sendInputText(mobileLine + '\r')
          setMobileLine('')
        }}
      >
        <input
          value={mobileLine}
          onChange={(e) => setMobileLine(e.target.value)}
          placeholder="Type command or keys for mobile Safari…"
          autoCapitalize="off"
          autoComplete="off"
          autoCorrect="off"
          spellCheck={false}
          enterKeyHint="send"
          aria-label="Mobile terminal command input"
        />
        <button type="submit" className="btn primary">Send</button>
      </form>
    </div>
  )
}
