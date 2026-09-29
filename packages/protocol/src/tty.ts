/**
 * Small interactive prompts shared by the hub and node command lines. Input for secrets is never echoed, so tokens do
 * not end up on screen, in screen recordings or in terminal scrollback.
 */
import { createInterface } from "node:readline";

export interface AskOptions {
  /** Do not echo what is typed. Use for tokens and passwords. */
  hidden?: boolean;
  default?: string;
}

/** True when both ends are a terminal, so prompting makes sense. */
export const isInteractive = (): boolean => !!process.stdin.isTTY && !!process.stdout.isTTY;

export function ask(question: string, opts: AskOptions = {}): Promise<string> {
  const label = opts.default ? `${question} [${opts.default}]: ` : `${question}: `;
  return new Promise((resolve) => {
    if (!opts.hidden || !process.stdin.isTTY) {
      const rl = createInterface({ input: process.stdin, output: process.stdout, terminal: !!process.stdin.isTTY });
      rl.question(label, (a) => {
        rl.close();
        resolve(a.trim() || opts.default || "");
      });
      return;
    }
    // Hidden entry: read raw keystrokes and echo nothing.
    process.stdout.write(label);
    const stdin = process.stdin;
    stdin.setRawMode(true);
    stdin.resume();
    stdin.setEncoding("utf8");
    let value = "";
    const onData = (chunk: string) => {
      for (const ch of chunk) {
        if (ch === "\r" || ch === "\n" || ch === "\u0004") {
          stdin.setRawMode(false);
          stdin.pause();
          stdin.off("data", onData);
          process.stdout.write("\n");
          resolve(value.trim() || opts.default || "");
          return;
        }
        if (ch === "\u0003") {
          stdin.setRawMode(false);
          process.stdout.write("\n");
          process.exit(130);
        }
        if (ch === "\u007f" || ch === "\b") value = value.slice(0, -1);
        else if (ch >= " ") value += ch;
      }
    };
    stdin.on("data", onData);
  });
}
