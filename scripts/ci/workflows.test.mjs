/**
 * Checks on the workflow files. GitHub Actions cannot be run locally, so this catches the mistakes that can be caught
 * without it: a malformed file, an unpinned action, a way to inject shell commands, a job that waits on one that does
 * not exist, and a command that does not exist in this repository.
 */
import { describe, expect, it } from "vitest";
import { existsSync, readFileSync, readdirSync } from "node:fs";
import { join } from "node:path";
import { parse } from "yaml";

const dir = ".github/workflows";
const files = readdirSync(dir).filter((f) => f.endsWith(".yml"));
const workflows = Object.fromEntries(files.map((f) => [f, parse(readFileSync(join(dir, f), "utf8"))]));
const pkg = JSON.parse(readFileSync("package.json", "utf8"));
const steps = (w) => Object.values(w.jobs).flatMap((j) => j.steps ?? []);
const runs = (w) =>
  steps(w)
    .filter((s) => s.run)
    .map((s) => s.run);

describe("every workflow", () => {
  it("is found", () => expect(files.sort()).toEqual(["ci.yml", "pr.yml", "release.yml", "security.yml"]));

  for (const [name, w] of Object.entries(workflows)) {
    describe(name, () => {
      it("has a name, triggers and jobs", () => {
        expect(typeof w.name).toBe("string");
        expect(w.on).toBeTruthy();
        expect(Object.keys(w.jobs).length).toBeGreaterThan(0);
      });

      it("sets permissions at the top, so jobs start with the least access", () => {
        expect(w.permissions).toBeTruthy();
      });

      it("gives every job a timeout, so a hang cannot burn hours", () => {
        for (const [id, j] of Object.entries(w.jobs)) expect(j["timeout-minutes"], id).toBeGreaterThan(0);
      });

      it("never uses pull_request_target, which would run untrusted code with secrets", () => {
        expect(JSON.stringify(w.on)).not.toContain("pull_request_target");
      });

      it("only waits on jobs that exist", () => {
        for (const [id, j] of Object.entries(w.jobs)) {
          for (const n of [j.needs ?? []].flat()) expect(w.jobs[n], `${id} needs ${n}`).toBeDefined();
        }
      });

      it("pins every action to a version", () => {
        for (const s of steps(w)) if (s.uses) expect(s.uses, s.uses).toMatch(/@[\w.-]+$/);
      });

      it("does not put event data straight into a shell command, where it could inject code", () => {
        for (const r of runs(w))
          expect(r, r).not.toMatch(/\$\{\{\s*github\.event\.(pull_request|issue|comment|head_commit|commits)/);
      });

      it("uses only commands that exist in this repository", () => {
        for (const r of runs(w)) {
          for (const m of r.matchAll(/pnpm (?:run )?([a-z][a-z0-9:-]*)/g)) {
            const builtin = ["install", "add", "audit", "exec", "dlx", "version"];
            if (builtin.includes(m[1]) && m[1] !== "version") continue;
            expect(Object.keys(pkg.scripts), `pnpm ${m[1]}`).toContain(m[1]);
          }
          for (const m of r.matchAll(/node (scripts\/[\w./-]+\.mjs)/g)) expect(existsSync(m[1]), m[1]).toBe(true);
        }
      });
    });
  }
});

describe("ci.yml", () => {
  const ci = workflows["ci.yml"];

  it("tests on Linux, Linux arm, macOS Apple silicon, macOS Intel and Windows", () => {
    const os = ci.jobs.test.strategy.matrix.include.map((e) => e.os);
    for (const o of ["ubuntu-latest", "ubuntu-24.04-arm", "macos-latest", "macos-15-intel", "windows-latest"])
      expect(os).toContain(o);
  });

  it("covers both supported Node lines", () => {
    expect(new Set(ci.jobs.test.strategy.matrix.include.map((e) => e.node))).toEqual(new Set([22, 24]));
  });

  it("does not stop the whole matrix when one platform fails, so every platform reports", () => {
    expect(ci.jobs.test.strategy["fail-fast"]).toBe(false);
  });

  it("marks only native Windows as allowed to fail", () => {
    const exp = ci.jobs.test.strategy.matrix.include.filter((e) => e.experimental).map((e) => e.os);
    expect(exp).toEqual(["windows-latest"]);
  });

  it("installs tmux where it exists, and skips the tmux tests on Windows", () => {
    const text = JSON.stringify(ci.jobs.test.steps);
    expect(text).toContain("apt-get install -y tmux");
    expect(text).toContain("brew install tmux");
    const e2e = ci.jobs.test.steps.find((s) => s.name?.startsWith("End to end"));
    expect(e2e.if).toContain("Windows");
  });

  it("smoke tests the packaged CLI on every platform", () => {
    expect(runs(ci).some((r) => r.includes("smoke-package.mjs"))).toBe(true);
  });

  it("enforces coverage, diff coverage and the performance budget", () => {
    const all = runs(ci).join("\n");
    expect(all).toContain("test:coverage");
    expect(all).toContain("diff-coverage");
    expect(all).toContain("--assert");
    expect(all).toContain("k6 run");
    expect(all).toContain("k6-delivery");
  });

  it("has one job to require in branch protection that waits on all the others", () => {
    const others = Object.keys(ci.jobs).filter((j) => j !== "ci-ok");
    expect(ci.jobs["ci-ok"].needs.sort()).toEqual(others.sort());
    expect(ci.jobs["ci-ok"].if).toBe("always()");
  });

  it("keeps the coverage thresholds in the workflow summary in step with the config", () => {
    const cfg = readFileSync("vitest.config.ts", "utf8");
    const wf = steps(ci).find((s) => s.env?.COVERAGE_THRESHOLDS).env.COVERAGE_THRESHOLDS;
    const t = JSON.parse(wf);
    for (const [k, v] of Object.entries(t)) expect(cfg).toMatch(new RegExp(`${k}:\\s*${v}\\b`));
  });
});

describe("pr.yml", () => {
  const pr = workflows["pr.yml"];

  it("checks the title, the checklist, tests and changesets", () => {
    const all = runs(pr).join("\n");
    for (const c of ["run.mjs title", "run.mjs checklist", "run.mjs tests", "run.mjs changeset"])
      expect(all).toContain(c);
  });

  it("passes the title through the environment, not the command line", () => {
    const step = steps(pr).find((s) => s.run?.includes("run.mjs title"));
    expect(step.env.PR_TITLE).toContain("github.event.pull_request.title");
    expect(step.run).not.toContain("github.event");
  });

  it("fetches full history where it diffs, because a shallow clone cannot", () => {
    for (const id of ["tests", "changeset"]) expect(pr.jobs[id].steps[0].with["fetch-depth"]).toBe(0);
  });

  it("reruns when labels change, so adding no-tests or no-changeset takes effect", () => {
    expect(pr.on.pull_request.types).toEqual(expect.arrayContaining(["labeled", "unlabeled", "edited", "synchronize"]));
  });

  it("has one job to require that waits on all the others", () => {
    const others = Object.keys(pr.jobs).filter((j) => j !== "pr-ok");
    expect(pr.jobs["pr-ok"].needs.sort()).toEqual(others.sort());
  });
});

describe("security.yml", () => {
  const sec = workflows["security.yml"];

  it("runs CodeQL, an audit, dependency review and a secret scan, and weekly", () => {
    expect(Object.keys(sec.jobs).sort()).toEqual(["audit", "codeql", "dependency-review", "secrets"]);
    expect(sec.on.schedule).toBeTruthy();
  });

  it("fails on production vulnerabilities", () => {
    expect(runs(sec).some((r) => r.includes("pnpm audit --prod"))).toBe(true);
  });

  it("gives write access only to the jobs that need it", () => {
    expect(sec.permissions).toEqual({ contents: "read" });
    expect(sec.jobs.codeql.permissions["security-events"]).toBe("write");
  });
});

describe("release.yml", () => {
  const rel = workflows["release.yml"];

  it("publishes with provenance, on pushes to main only", () => {
    expect(rel.on.push.branches).toEqual(["main"]);
    const step = steps(rel).find((s) => s.uses?.startsWith("changesets/action"));
    expect(step.env.NPM_CONFIG_PROVENANCE).toBe("true");
    expect(rel.permissions["id-token"]).toBe("write");
  });

  it("builds before it publishes", () => {
    expect(pkg.scripts.release).toContain("pnpm build");
  });
});

describe("repository files the workflows rely on", () => {
  it.each([
    ".github/dependabot.yml",
    ".github/CODEOWNERS",
    ".github/pull_request_template.md",
    ".github/ISSUE_TEMPLATE/bug_report.yml",
    ".github/ISSUE_TEMPLATE/feature_request.yml",
    ".github/ISSUE_TEMPLATE/config.yml",
    ".changeset/config.json",
    "SECURITY.md",
    "CONTRIBUTING.md",
  ])("has %s", (f) => expect(existsSync(f), f).toBe(true));

  it("has a pull request template whose checklist the checklist check can read", () => {
    const body = readFileSync(".github/pull_request_template.md", "utf8");
    expect((body.match(/^- \[ \]/gm) ?? []).length).toBeGreaterThanOrEqual(3);
  });

  it("keeps Dependabot on both ecosystems", () => {
    const d = parse(readFileSync(".github/dependabot.yml", "utf8"));
    expect(d.updates.map((u) => u["package-ecosystem"]).sort()).toEqual(["github-actions", "npm"]);
  });
});
