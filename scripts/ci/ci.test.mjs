import { describe, expect, it } from "vitest";
import { execFileSync } from "node:child_process";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import {
  coverageMarkdown,
  evaluateChangeset,
  evaluateChecklist,
  evaluateDiffCoverage,
  evaluateK6,
  evaluateTests,
  evaluateTitle,
  isSource,
  isTest,
  parseAddedLines,
  parseLcov,
} from "./lib.mjs";

describe("file classification", () => {
  it("knows source from tests", () => {
    expect(isSource("packages/hub/src/hub.ts")).toBe(true);
    expect(isSource("packages/hub/src/types.d.ts")).toBe(false);
    expect(isSource("packages/hub/test/hub.test.ts")).toBe(false);
    expect(isSource("packages/hub/bench/load.ts")).toBe(false);
    expect(isSource("docs/SETUP.md")).toBe(false);
    expect(isTest("packages/node/test/e2e.e2e.test.ts")).toBe(true);
    expect(isTest("scripts/ci/ci.test.mjs")).toBe(true);
    expect(isTest("packages/node/src/files.ts")).toBe(false);
  });
});

describe("tests required", () => {
  it("passes when nothing in source changed", () => {
    expect(evaluateTests({ changed: ["README.md", "docs/SETUP.md", ".github/workflows/ci.yml"] }).ok).toBe(true);
  });
  it("passes when source and tests changed together", () => {
    expect(evaluateTests({ changed: ["packages/hub/src/hub.ts", "packages/hub/test/hub.test.ts"] }).ok).toBe(true);
  });
  it("fails when source changed alone, and names the files", () => {
    const r = evaluateTests({ changed: ["packages/hub/src/hub.ts", "packages/node/src/files.ts"] });
    expect(r.ok).toBe(false);
    expect(r.message).toContain("packages/hub/src/hub.ts");
    expect(r.message).toContain("no-tests");
  });
  it("lets the label through", () => {
    expect(evaluateTests({ changed: ["packages/hub/src/hub.ts"], labels: ["no-tests"] }).ok).toBe(true);
  });
  it("does not count a type declaration as source", () => {
    expect(evaluateTests({ changed: ["packages/hub/src/x.d.ts"] }).ok).toBe(true);
  });
});

describe("changeset required", () => {
  it("is not needed for the private hub, docs or tests", () => {
    expect(
      evaluateChangeset({ changed: ["packages/hub/src/hub.ts", "docs/a.md", "packages/node/test/a.test.ts"] }).ok,
    ).toBe(true);
  });
  it("is needed for code that ships", () => {
    for (const f of [
      "packages/node/src/cli.ts",
      "packages/protocol/src/index.ts",
      "packages/agent-tools/src/mcp.ts",
      "packages/node/package.json",
    ]) {
      expect(evaluateChangeset({ changed: [f] }).ok, f).toBe(false);
    }
  });
  it("passes with a new changeset file, but not the readme or config", () => {
    const changed = ["packages/node/src/cli.ts", ".changeset/brave-owls-sing.md"];
    expect(evaluateChangeset({ changed, added: [".changeset/brave-owls-sing.md"] }).ok).toBe(true);
    expect(
      evaluateChangeset({
        changed: ["packages/node/src/cli.ts", ".changeset/README.md"],
        added: [".changeset/README.md"],
      }).ok,
    ).toBe(false);
    expect(
      evaluateChangeset({
        changed: ["packages/node/src/cli.ts", ".changeset/config.json"],
        added: [".changeset/config.json"],
      }).ok,
    ).toBe(false);
  });
  it("only counts a changeset that this change added, not an old one that was edited", () => {
    expect(evaluateChangeset({ changed: ["packages/node/src/cli.ts", ".changeset/old.md"], added: [] }).ok).toBe(false);
  });
  it("lets the label through", () => {
    expect(evaluateChangeset({ changed: ["packages/node/src/cli.ts"], labels: ["no-changeset"] }).ok).toBe(true);
  });
});

describe("pull request title", () => {
  it.each([
    "feat: add web chat",
    "fix(node): stop reading stale prompts from scrollback",
    "docs: explain pairing",
    "feat(hub)!: drop environment configuration",
    "chore(deps): bump vitest",
    "perf(hub): index agents by project",
  ])("accepts %s", (t) => expect(evaluateTitle(t).ok).toBe(true));

  it.each([
    "Add web chat",
    "feat add web chat",
    "feat:",
    "feat: ",
    "feature: add thing",
    "Fix: capital type",
    "fix(Node): capital scope",
    "fix(): empty scope",
    `feat: ${"x".repeat(100)}`,
  ])("rejects %j", (t) => expect(evaluateTitle(t).ok).toBe(false));

  it("shows an example when it rejects", () => {
    expect(evaluateTitle("oops").message).toContain("Example:");
  });
});

describe("checklist", () => {
  it("passes when everything is ticked, or the list is empty", () => {
    expect(evaluateChecklist("- [x] Tests\n- [X] Docs").ok).toBe(true);
    expect(evaluateChecklist("no checklist here").ok).toBe(true);
    expect(evaluateChecklist(undefined).ok).toBe(true);
  });
  it("fails and lists what is left", () => {
    const r = evaluateChecklist("## Checklist\n- [x] Tests\n- [ ] Docs updated\n  * [ ] nested one");
    expect(r.ok).toBe(false);
    expect(r.message).toContain("Docs updated");
    expect(r.message).toContain("nested one");
  });
});

const DIFF = `diff --git a/packages/hub/src/a.ts b/packages/hub/src/a.ts
index 111..222 100644
--- a/packages/hub/src/a.ts
+++ b/packages/hub/src/a.ts
@@ -10,0 +11,3 @@ function x() {
+  a();
+  b();
+  c();
@@ -40 +43 @@
-  old();
+  d();
diff --git a/packages/hub/src/gone.ts b/packages/hub/src/gone.ts
deleted file mode 100644
--- a/packages/hub/src/gone.ts
+++ /dev/null
@@ -1,2 +0,0 @@
-x
-y
diff --git a/docs/a.md b/docs/a.md
--- a/docs/a.md
+++ b/docs/a.md
@@ -1 +1,2 @@
+text
`;

describe("diff parsing", () => {
  it("finds the added line numbers per file", () => {
    const m = parseAddedLines(DIFF);
    expect([...m.get("packages/hub/src/a.ts")].sort((a, b) => a - b)).toEqual([11, 12, 13, 43]);
    expect(m.has("packages/hub/src/gone.ts")).toBe(false);
    expect([...m.get("docs/a.md")]).toEqual([1, 2]);
  });
});

const LCOV = `TN:
SF:packages/hub/src/a.ts
DA:11,1
DA:12,0
DA:13,4
DA:20,0
end_of_record
SF:packages/hub/src/other.ts
DA:1,1
end_of_record
`;

describe("lcov parsing", () => {
  it("reads hits per line", () => {
    const m = parseLcov(LCOV);
    expect(m.get("packages/hub/src/a.ts").get(12)).toBe(0);
    expect(m.get("packages/hub/src/a.ts").get(13)).toBe(4);
    expect(m.size).toBe(2);
  });
});

describe("diff coverage", () => {
  const added = parseAddedLines(DIFF);
  const lcov = parseLcov(LCOV);

  it("counts only added executable lines", () => {
    const r = evaluateDiffCoverage({ added, lcov, threshold: 50 });
    // Lines 11, 12, 13 are executable. 43 is not in the report. Two of three ran.
    expect(r.rows).toEqual([{ file: "packages/hub/src/a.ts", executable: 3, covered: 2, pct: 67 }]);
    expect(r.pct).toBe(66.7);
    expect(r.ok).toBe(true);
  });

  it("fails below the threshold and says which file", () => {
    const r = evaluateDiffCoverage({ added, lcov, threshold: 80 });
    expect(r.ok).toBe(false);
    expect(r.message).toContain("only 66.7% covered");
    expect(r.message).toContain("packages/hub/src/a.ts");
  });

  it("passes when there is nothing executable to cover", () => {
    const r = evaluateDiffCoverage({
      added: parseAddedLines("+++ b/packages/hub/src/new.ts\n@@ -0,0 +1 @@\n+x\n"),
      lcov,
      threshold: 80,
    });
    expect(r.ok).toBe(true);
  });

  it("matches the file when lcov paths are absolute", () => {
    const abs = parseLcov(
      "SF:/home/runner/work/claudeCord/claudeCord/packages/hub/src/a.ts\nDA:11,1\nDA:12,1\nDA:13,1\nend_of_record\n",
    );
    expect(evaluateDiffCoverage({ added, lcov: abs, threshold: 100 }).ok).toBe(true);
  });

  it("ignores files that are not source", () => {
    expect(
      evaluateDiffCoverage({ added: parseAddedLines("+++ b/docs/a.md\n@@ -1 +1 @@\n+x\n"), lcov, threshold: 100 }).ok,
    ).toBe(true);
  });
});

describe("k6 delivery", () => {
  const ok = { sent: 1000, expected: 3000, delivered: 3000, registered: 400, p99: 2 };
  it("passes when everything expected was delivered", () => expect(evaluateK6(ok).ok).toBe(true));
  it("fails below the minimum", () => {
    const r = evaluateK6({ ...ok, delivered: 2900 });
    expect(r.ok).toBe(false);
    expect(r.message).toContain("96.67%");
  });
  it("fails when nothing was measured", () => {
    expect(evaluateK6({ ...ok, sent: 0 }).ok).toBe(false);
    expect(evaluateK6({ ...ok, expected: 0 }).ok).toBe(false);
  });
  it("fails when too few agents registered", () => {
    expect(evaluateK6(ok, { agents: 500 }).ok).toBe(false);
    expect(evaluateK6(ok, { agents: 400 }).ok).toBe(true);
  });
});

describe("coverage summary", () => {
  it("renders a table", () => {
    const md = coverageMarkdown(
      {
        total: {
          statements: { pct: 80, covered: 8, total: 10 },
          branches: { pct: 70, covered: 7, total: 10 },
          functions: { pct: 90, covered: 9, total: 10 },
          lines: { pct: 85, covered: 17, total: 20 },
        },
      },
      { lines: 75 },
    );
    expect(md).toContain("| Lines | 85% | 75% | 17/20 |");
    expect(md).toContain("| Branches | 70% | -% | 7/10 |");
  });
});

describe("the runner against a real git history", () => {
  const run = fileURLToPath(new URL("./run.mjs", import.meta.url));

  function repo(steps) {
    const dir = mkdtempSync(join(tmpdir(), "cc-ci-"));
    const g = (...a) =>
      execFileSync("git", a, {
        cwd: dir,
        encoding: "utf8",
        env: {
          ...process.env,
          GIT_AUTHOR_NAME: "t",
          GIT_AUTHOR_EMAIL: "t@t",
          GIT_COMMITTER_NAME: "t",
          GIT_COMMITTER_EMAIL: "t@t",
        },
      });
    g("init", "-q", "-b", "main");
    mkdirSync(join(dir, "packages/hub/src"), { recursive: true });
    mkdirSync(join(dir, "packages/hub/test"), { recursive: true });
    writeFileSync(join(dir, "packages/hub/src/a.ts"), "export const a = 1;\n");
    g("add", "-A");
    g("commit", "-qm", "base");
    const base = g("rev-parse", "HEAD").trim();
    steps(dir);
    g("add", "-A");
    g("commit", "-q", "--allow-empty", "-m", "change");
    return { dir, base, head: g("rev-parse", "HEAD").trim() };
  }

  const exec = (r, check, extra = {}) => {
    try {
      const out = execFileSync(process.execPath, [run, check], {
        cwd: r.dir,
        encoding: "utf8",
        env: { ...process.env, BASE_SHA: r.base, HEAD_SHA: r.head, GITHUB_EVENT_PATH: "", ...extra },
      });
      return { code: 0, out };
    } catch (e) {
      return { code: e.status, out: String(e.stdout) };
    }
  };

  it("fails a source-only change and passes it once a test is added", () => {
    const r1 = repo((d) => writeFileSync(join(d, "packages/hub/src/a.ts"), "export const a = 2;\n"));
    expect(exec(r1, "tests").code).toBe(1);
    const r2 = repo((d) => {
      writeFileSync(join(d, "packages/hub/src/a.ts"), "export const a = 2;\n");
      writeFileSync(join(d, "packages/hub/test/a.test.ts"), "// test\n");
    });
    expect(exec(r2, "tests").code).toBe(0);
    rmSync(r1.dir, { recursive: true, force: true });
    rmSync(r2.dir, { recursive: true, force: true });
  });

  it("requires a changeset for shipped code and accepts a new one", () => {
    const r1 = repo((d) => {
      mkdirSync(join(d, "packages/node/src"), { recursive: true });
      writeFileSync(join(d, "packages/node/src/cli.ts"), "export {};\n");
    });
    expect(exec(r1, "changeset").code).toBe(1);
    const r2 = repo((d) => {
      mkdirSync(join(d, "packages/node/src"), { recursive: true });
      mkdirSync(join(d, ".changeset"), { recursive: true });
      writeFileSync(join(d, "packages/node/src/cli.ts"), "export {};\n");
      writeFileSync(join(d, ".changeset/calm-dogs.md"), '---\n"claudecord": patch\n---\n\nfix\n');
    });
    expect(exec(r2, "changeset").code).toBe(0);
    rmSync(r1.dir, { recursive: true, force: true });
    rmSync(r2.dir, { recursive: true, force: true });
  });

  it("measures coverage of only the lines the change added", () => {
    const r = repo((d) =>
      writeFileSync(
        join(d, "packages/hub/src/a.ts"),
        "export const a = 1;\nexport function f() {\n  return 1;\n}\nexport function g() {\n  return 2;\n}\n",
      ),
    );
    // Lines 2 to 7 are new. Report f as run and g as never run.
    writeFileSync(join(r.dir, "lcov.info"), "SF:packages/hub/src/a.ts\nDA:1,1\nDA:3,1\nDA:6,0\nend_of_record\n");
    const low = execFileSync(process.execPath, [run, "diff-coverage", "lcov.info"], {
      cwd: r.dir,
      encoding: "utf8",
      env: { ...process.env, BASE_SHA: r.base, HEAD_SHA: r.head, DIFF_COVERAGE: "40", GITHUB_EVENT_PATH: "" },
    });
    expect(low).toContain("50% covered");
    expect(() =>
      execFileSync(process.execPath, [run, "diff-coverage", "lcov.info"], {
        cwd: r.dir,
        stdio: "pipe",
        env: { ...process.env, BASE_SHA: r.base, HEAD_SHA: r.head, DIFF_COVERAGE: "80", GITHUB_EVENT_PATH: "" },
      }),
    ).toThrow();
    rmSync(r.dir, { recursive: true, force: true });
  });

  it("explains itself when called wrongly", () => {
    const r = repo(() => {});
    expect(exec(r, "nonsense").code).toBe(2);
    rmSync(r.dir, { recursive: true, force: true });
  });
});
