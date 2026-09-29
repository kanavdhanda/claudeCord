/**
 * Everything pasted into an agent's terminal passes through here. Peer messages, file notices and human replies are
 * text the agent will read as input, so the goal is that no message can pose as someone else or escape the paste.
 */

/**
 * Removes control characters, keeping newline and tab. This matters because a message containing the bracketed paste
 * end marker (ESC [ 2 0 1 ~) would end the paste early, and whatever followed would be typed as keystrokes. Removing
 * ESC and the C1 range (which includes the single byte CSI) also blocks cursor and screen manipulation.
 */
export function stripControl(text: string): string {
  return text.replace(/[\u0000-\u0008\u000b-\u001f\u007f-\u009f\u2028\u2029\u202a-\u202e\u2066-\u2069]/g, "");
}

/**
 * Quotes every line after the first. A message can then never contain a line that looks like another sender's header
 * such as "[engineer] ...", because only the first line carries the real header added by the hub.
 */
export function quoteBody(text: string): string {
  const [first = "", ...rest] = stripControl(text).split("\n");
  return [first, ...rest.map((l) => `> ${l}`)].join("\n");
}

export interface Delivery {
  from: string;
  text: string;
  thread?: string;
  /** Hub delivery id, reported back once the agent starts on it. */
  msgId?: string;
}

export function formatDeliveries(items: Delivery[]): string {
  return items
    .map(
      (d) => `[${stripControl(d.from)}${d.thread ? ` | thread: ${stripControl(d.thread)}` : ""}] ${quoteBody(d.text)}`,
    )
    .join("\n\n");
}
