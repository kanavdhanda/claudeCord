/**
 * Logic behind the pull request checks. Kept free of I/O so it can be tested. The small scripts next to this file
 * read the environment and git, call these functions, and set the exit code.
 */

/** Packages whose source ends up in a published npm package, so a change needs a changeset. */
export const PUBLISHED = ["packages/node/", "packages/protocol/", "packages/agent-tools/"];

const SRC = /^packages\/[^/]+\/src\/.+\.(ts|mts|js|mjs)$/;
const TEST = /^(packages\/[^/]+\/test\/|scripts\/.+\.test\.)/;

export const isSource = (f) => SRC.test(f) && !f.endsWith(".d.ts");
export const isTest = (f) => TEST.test(f);

/** Source changes need test changes, unless the pull request is labelled `no-tests`. */
export function evaluateTests({ changed, labels = [] }) {
  const src = changed.filter(isSource);
  if (!src.length) return { ok: true, message: "No source files changed, so no tests are required." };
  if (changed.some(isTest))
    return { ok: true, message: `Source changed (${src.length} file(s)) and tests changed too.` };
  if (labels.includes("no-tests"))
    return { ok: true, message: 'Source changed with no tests, allowed by the "no-tests" label.' };
  return {
    ok: false,
    message: [
      "Source files changed but no tests were added or updated:",
      ...src.map((f) => `  ${f}`),
      "",
      'Add a test that fails without this change. If it truly cannot be tested (for example a rename), add the "no-tests" label and say why in the description.',
    ].join("\n"),
  };
}

/** A change to something that ships needs a changeset, a short note that becomes the release notes. */
export function evaluateChangeset({ changed, added = changed, labels = [] }) {
  const touched = changed.filter(
    (f) => PUBLISHED.some((p) => f.startsWith(p)) && (isSource(f) || f.endsWith("package.json")),
  );
  if (!touched.length) return { ok: true, message: "Nothing that ships to npm changed, so no changeset is required." };
  const sets = added.filter((f) => /^\.changeset\/[^/]+\.md$/.test(f) && !f.endsWith("README.md"));
  if (sets.length) return { ok: true, message: `Changeset present: ${sets.join(", ")}` };
  if (labels.includes("no-changeset"))
    return { ok: true, message: 'No changeset, allowed by the "no-changeset" label.' };
  return {
    ok: false,
    message: [
      "This changes code that ships to npm, but there is no changeset:",
      ...touched.slice(0, 10).map((f) => `  ${f}`),
      "",
      "Run `pnpm changeset`, choose the bump, and describe the change for users. Commit the file it creates.",
      'If users are not affected (tests, tooling, docs), add the "no-changeset" label.',
    ].join("\n"),
  };
}

const TYPES = ["feat", "fix", "docs", "test", "refactor", "perf", "chore", "ci", "build", "style", "revert"];
const TITLE = new RegExp(`^(${TYPES.join("|")})(\\([a-z0-9][a-z0-9/_-]*\\))?!?: \\S.{2,}$`);

export function evaluateTitle(title) {
  if (title.length > 100)
    return { ok: false, message: `Title is ${title.length} characters. Keep it to 100 or fewer.` };
  if (TITLE.test(title)) return { ok: true, message: "Title follows Conventional Commits." };
  return {
    ok: false,
    message: `"${title}" does not follow Conventional Commits.\nUse: <type>(<optional scope>): <summary>, where type is one of ${TYPES.join(", ")}.\nExample: fix(node): stop reading stale prompts from scrollback`,
  };
}

/** Every checkbox the template put in the description must be ticked, or the line removed if it does not apply. */
export function evaluateChecklist(body) {
  const unchecked = (body ?? "").split("\n").filter((l) => /^\s*[-*]\s+\[ \]/.test(l));
  if (!unchecked.length) return { ok: true, message: "Checklist complete." };
  return {
    ok: false,
    message: [
      "These checklist items are not ticked:",
      ...unchecked.map((l) => `  ${l.trim()}`),
      "",
      "Tick each one, or delete the line if it does not apply and say why.",
    ].join("\n"),
  };
}

/** Added line numbers per file from `git diff -U0` output. */
export function parseAddedLines(diff) {
  const out = new Map();
  let file = null;
  for (const line of diff.split("\n")) {
    const f = line.match(/^\+\+\+ b\/(.+)$/);
    if (f) {
      file = f[1];
      continue;
    }
    if (line.startsWith("+++ ")) file = null;
    const h = line.match(/^@@ -\d+(?:,\d+)? \+(\d+)(?:,(\d+))? @@/);
    if (h && file) {
      const start = Number(h[1]);
      const count = h[2] === undefined ? 1 : Number(h[2]);
      const set = out.get(file) ?? new Set();
      for (let i = 0; i < count; i++) set.add(start + i);
      out.set(file, set);
    }
  }
  return out;
}

/** Per file, which lines are executable and how many times they ran, from an lcov report. */
export function parseLcov(lcov) {
  const out = new Map();
  let file = null;
  for (const line of lcov.split("\n")) {
    if (line.startsWith("SF:")) {
      file = line.slice(3).replace(/\\/g, "/");
      out.set(file, new Map());
    } else if (line.startsWith("DA:") && file) {
      const [n, hits] = line.slice(3).split(",");
      out.get(file).set(Number(n), Number(hits));
    } else if (line === "end_of_record") file = null;
  }
  return out;
}

/** Coverage of the lines this change added, so old untested code does not block new tested code. */
export function evaluateDiffCoverage({ added, lcov, threshold = 80, root = "" }) {
  const rows = [];
  let executable = 0;
  let covered = 0;
  for (const [file, lines] of added) {
    if (!isSource(file)) continue;
    const key = [...lcov.keys()].find((k) => k === file || k === `${root}/${file}` || k.endsWith(`/${file}`));
    if (!key) continue;
    const da = lcov.get(key);
    let e = 0;
    let c = 0;
    for (const n of lines) {
      if (!da.has(n)) continue;
      e++;
      if (da.get(n) > 0) c++;
    }
    if (e) rows.push({ file, executable: e, covered: c, pct: Math.round((c / e) * 100) });
    executable += e;
    covered += c;
  }
  if (!executable) return { ok: true, pct: 100, rows, message: "No new executable lines to cover." };
  const pct = Math.round((covered / executable) * 1000) / 10;
  const table = rows.map((r) => `  ${String(r.pct).padStart(3)}%  ${r.covered}/${r.executable}  ${r.file}`).join("\n");
  const ok = pct >= threshold;
  return {
    ok,
    pct,
    rows,
    message: `${ok ? "New code is" : "New code is only"} ${pct}% covered (${covered}/${executable} lines, threshold ${threshold}%).\n${table}`,
  };
}

/** Markdown for the job summary page. */
export function coverageMarkdown(summary, thresholds) {
  const t = summary.total;
  const row = (name, key) =>
    `| ${name} | ${t[key].pct}% | ${thresholds?.[key] ?? "-"}% | ${t[key].covered}/${t[key].total} |`;
  return [
    "## Coverage",
    "",
    "| | Covered | Minimum | Lines |",
    "|-|-|-|-|",
    row("Statements", "statements"),
    row("Branches", "branches"),
    row("Functions", "functions"),
    row("Lines", "lines"),
    "",
  ].join("\n");
}

/** The k6 run must deliver nearly everything it expected to, which k6's own thresholds cannot express. */
export function evaluateK6({ sent, expected, delivered, registered, p99 }, { minPct = 99.5, agents } = {}) {
  if (!expected || !sent) return { ok: false, message: "k6 sent no messages, so nothing was measured." };
  if (agents && registered < agents)
    return { ok: false, message: `Only ${registered} of ${agents} agents registered.` };
  const pct = (delivered / expected) * 100;
  const ok = pct >= minPct;
  return {
    ok,
    message: `${delivered} of ${expected} expected messages delivered (${pct.toFixed(2)}%, minimum ${minPct}%), p99 ${Number(p99).toFixed(1)} ms.`,
  };
}
