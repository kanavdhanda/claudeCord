// The agent programs the dashboard offers. The first three have readers of their own; the rest are started by one word and read by what the
// screen looks like. Keep in step with HARNESSES in crates/claudecord-protocol/src/harness.rs.
const NAMED = ['gemini', 'opencode', 'aider', 'cursor-agent', 'copilot', 'amp', 'goose', 'qwen', 'crush', 'droid']

export function HarnessOptions({ withGeneric = false }: { withGeneric?: boolean }) {
  return (
    <>
      <option value="claude">Claude Code</option>
      <option value="codex">Codex</option>
      <option value="agy">agy</option>
      <optgroup label="Other programs">
        {NAMED.map((n) => (
          <option key={n} value={n}>
            {n}
          </option>
        ))}
        {withGeneric && <option value="generic">Any other program (generic)</option>}
      </optgroup>
    </>
  )
}
