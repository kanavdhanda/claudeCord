import path from "node:path";

// Commands that still need a human "yes" even after a plan has been approved.
const DANGEROUS_BASH: RegExp[] = [
  /\bsudo\b/,
  /\brm\s+(-[a-zA-Z]*[rf][a-zA-Z]*\s+)+(\/|~|\$HOME|\*)/,
  /\bgit\s+push\b.*(--force|-f\b|--force-with-lease)/,
  /\bgit\s+reset\s+--hard\b/,
  /\bgit\s+clean\s+-[a-zA-Z]*f/,
  /(curl|wget)\b[^|]*\|\s*(sudo\s+)?(ba|z)?sh\b/,
  /\bmkfs\b|\bdd\s+.*of=\/dev\//,
  /\bchmod\s+-R\s+7?77\s+\//,
  /\b(shutdown|reboot|halt)\b/,
  /:\(\)\s*\{.*\};\s*:/, // fork bomb
  /\b(del|erase)\s+\/[sq]\b|\brd\s+\/s\b|\bformat\s+[a-z]:/i, // Windows
  /\bRemove-Item\b.*-Recurse.*-Force/i,
  />\s*\/etc\//,
];

export function isDangerousBash(command: string): boolean {
  return DANGEROUS_BASH.some((re) => re.test(command));
}

/** True when a file path resolves outside the project directory. */
export function isOutsideProject(filePath: string, projectPath: string): boolean {
  const root = path.resolve(projectPath);
  const target = path.resolve(root, filePath);
  return target !== root && !target.startsWith(root + path.sep);
}

/** Decide whether a tool call needs human approval after the plan was approved. */
export function needsApprovalDespitePlan(
  toolName: string,
  input: Record<string, unknown>,
  projectPath: string,
): boolean {
  if (toolName === "Bash") return isDangerousBash(String(input.command ?? ""));
  if (toolName === "Write" || toolName === "Edit" || toolName === "MultiEdit" || toolName === "NotebookEdit") {
    const p = input.file_path ?? input.notebook_path;
    return typeof p === "string" && isOutsideProject(p, projectPath);
  }
  return false;
}
