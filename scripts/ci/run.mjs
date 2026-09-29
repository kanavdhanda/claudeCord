#!/usr/bin/env node
/**
 * Entry point for the pull request checks.  node scripts/ci/run.mjs <check>
 * Reads the pull request from GITHUB_EVENT_PATH, and the diff from git using BASE_SHA and HEAD_SHA.
 */
import { execFileSync } from "node:child_process";
import { appendFileSync, existsSync, readFileSync } from "node:fs";
import {
  coverageMarkdown,
  evaluateChangeset,
  evaluateChecklist,
  evaluateDiffCoverage,
  evaluateK6,
  evaluateTests,
  evaluateTitle,
  parseAddedLines,
  parseLcov,
} from "./lib.mjs";

const git = (...args) => execFileSync("git", args, { encoding: "utf8", maxBuffer: 64 * 1024 * 1024 });
const event =
  process.env.GITHUB_EVENT_PATH && existsSync(process.env.GITHUB_EVENT_PATH)
    ? JSON.parse(readFileSync(process.env.GITHUB_EVENT_PATH, "utf8"))
    : {};
const pr = event.pull_request ?? {};
const labels = (pr.labels ?? []).map((l) => l.name);
const base = process.env.BASE_SHA ?? pr.base?.sha;
const head = process.env.HEAD_SHA ?? pr.head?.sha ?? "HEAD";

function changedFiles(filter = "ACMRT") {
  return git("diff", "--name-only", `--diff-filter=${filter}`, `${base}...${head}`).split("\n").filter(Boolean);
}

function finish(result) {
  console.log(result.message);
  const summary = process.env.GITHUB_STEP_SUMMARY;
  if (summary) appendFileSync(summary, `${result.ok ? "Passed" : "Failed"}: ${result.message}\n\n`);
  process.exit(result.ok ? 0 : 1);
}

const check = process.argv[2];
switch (check) {
  case "title":
    finish(evaluateTitle(pr.title ?? process.env.PR_TITLE ?? ""));
    break;
  case "checklist":
    finish(evaluateChecklist(pr.body ?? ""));
    break;
  case "tests":
    finish(evaluateTests({ changed: changedFiles(), labels }));
    break;
  case "changeset":
    finish(evaluateChangeset({ changed: changedFiles(), added: changedFiles("A"), labels }));
    break;
  case "diff-coverage": {
    const lcovPath = process.argv[3] ?? "coverage/lcov.info";
    if (!existsSync(lcovPath))
      finish({ ok: false, message: `${lcovPath} not found. Run the tests with coverage first.` });
    const added = parseAddedLines(git("diff", "-U0", "--diff-filter=ACMR", `${base}...${head}`, "--", "packages"));
    finish(
      evaluateDiffCoverage({
        added,
        lcov: parseLcov(readFileSync(lcovPath, "utf8")),
        threshold: Number(process.env.DIFF_COVERAGE ?? 80),
        root: process.cwd().replace(/\\/g, "/"),
      }),
    );
    break;
  }
  case "k6-delivery": {
    const path = process.argv[3] ?? "k6-summary.json";
    if (!existsSync(path)) finish({ ok: false, message: `${path} not found. Did k6 finish?` });
    finish(
      evaluateK6(JSON.parse(readFileSync(path, "utf8")), {
        minPct: Number(process.env.K6_MIN_DELIVERED ?? 99.5),
        agents: Number(process.env.K6_AGENTS) || undefined,
      }),
    );
    break;
  }
  case "coverage-summary": {
    const path = process.argv[3] ?? "coverage/coverage-summary.json";
    if (!existsSync(path)) finish({ ok: false, message: `${path} not found.` });
    const md = coverageMarkdown(
      JSON.parse(readFileSync(path, "utf8")),
      JSON.parse(process.env.COVERAGE_THRESHOLDS ?? "{}"),
    );
    console.log(md);
    if (process.env.GITHUB_STEP_SUMMARY) appendFileSync(process.env.GITHUB_STEP_SUMMARY, md);
    break;
  }
  default:
    console.error("usage: run.mjs title|checklist|tests|changeset|diff-coverage|coverage-summary|k6-delivery");
    process.exit(2);
}
