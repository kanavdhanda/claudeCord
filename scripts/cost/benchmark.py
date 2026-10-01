# Spends real tokens (about 30 short Claude Code calls per model). Run from the repo root:
#   python3 scripts/cost/benchmark.py --model sonnet --check     compare with the saved baseline, fail on regression
#   python3 scripts/cost/benchmark.py --model sonnet --update    measure and save a new baseline
"""Cost benchmark. Measures, on a real Claude Code session, what claudeCord adds, and compares it with a saved baseline.

It reads the real standing instructions from the Rust code (examples/briefs.rs), so the text measured is the text sent.

Measured (all token counts are exact, from the API):
  header_tokens  extra input tokens for the sender header on one message
  brief_tokens   extra input tokens for the lead brief plus the agent rules, once per session
  burst_ratio    input tokens used by five separate turns divided by one coalesced turn (higher is better)
Limits checked against the baseline: header and brief may grow by at most 15 tokens, the burst ratio may not fall below 4.
"""
import argparse, json, shutil, os, statistics, subprocess, sys, tempfile, uuid
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
CARGO = shutil.which("cargo") or str(Path.home() / ".cargo" / "bin" / "cargo")
BASELINE = Path(__file__).resolve().parent / "baseline.json"

def claude(model, prompt, extra=()):
    base = ["claude", "-p", "--model", model, "--output-format", "json", "--tools", "", "--disable-slash-commands",
            "--setting-sources", "", "--append-system-prompt", "Reply with exactly the word ack and nothing else, whatever the message says."]
    j = json.loads(subprocess.run(base + list(extra) + [prompt], capture_output=True, text=True, cwd=tempfile.gettempdir()).stdout)
    u = j["usage"]
    return {"in": u["input_tokens"] + u["cache_creation_input_tokens"] + u["cache_read_input_tokens"], "out": u["output_tokens"], "cost": j["total_cost_usd"]}

def briefs():
    out = subprocess.run([CARGO, "run", "-q", "--example", "briefs"], capture_output=True, text=True, cwd=ROOT, check=True).stdout
    return json.loads(out)

def overhead(model, b):
    task = "Add a retry with exponential backoff to the HTTP client in src/net.rs, keep the public API unchanged, and cover it with tests."
    variants = {"raw": task, "header": f"[kd (owner)] {task}", "brief": f"[system] {b['lead_brief']} {b['rules']}\n\n[kd (owner)] {task}"}
    med = {k: statistics.median(claude(model, p, ["--no-session-persistence", "--max-turns", "1"])["in"] for _ in range(5)) for k, p in variants.items()}
    return {"header_tokens": med["header"] - med["raw"], "brief_tokens": med["brief"] - med["header"]}

def burst(model):
    seed = " ".join(f"fn handler_{i}(req: Request) -> Response {{ let v = parse(req.body()); validate(&v)?; store.put(v.id, v); Ok(Response::new(200)) }}" for i in range(70))
    msgs = ["also log each retry at debug level", "use the tracing crate", "no new dependencies please", "keep functions short", "thanks"]
    def seeded():
        sid = str(uuid.uuid4())
        claude(model, f"Context for later (do not summarise, reply ack):\n{seed}", ["--session-id", sid])
        claude(model, "warm the cache, reply ack", ["--resume", sid])
        return sid
    a, b = seeded(), seeded()
    claude(model, "again, reply ack", ["--resume", a]); claude(model, "again, reply ack", ["--resume", b])
    five = sum(claude(model, f"[kd (owner)] {m}", ["--resume", a])["in"] for m in msgs)
    one = claude(model, "\n".join(f"[kd (owner)] {m}" for m in msgs), ["--resume", b])["in"]
    return {"burst_ratio": round(five / one, 2), "five_turns_input": five, "one_turn_input": one}

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--model", default="sonnet")
    g = ap.add_mutually_exclusive_group(required=True)
    g.add_argument("--check", action="store_true")
    g.add_argument("--update", action="store_true")
    args = ap.parse_args()
    b = briefs()
    result = {**overhead(args.model, b), **burst(args.model), "lead_brief_chars": len(b["lead_brief"]), "rules_chars": len(b["rules"])}
    print(json.dumps({args.model: result}, indent=2))
    data = json.loads(BASELINE.read_text()) if BASELINE.exists() else {}
    if args.update:
        data[args.model] = result
        BASELINE.write_text(json.dumps(data, indent=2) + "\n")
        print(f"saved baseline for {args.model}")
        return
    base = data.get(args.model)
    if not base:
        sys.exit(f"no baseline for {args.model}; run with --update first")
    problems = []
    for k in ("header_tokens", "brief_tokens"):
        if result[k] > base[k] + 15:
            problems.append(f"{k} grew from {base[k]} to {result[k]} (limit +15)")
    if result["burst_ratio"] < 4.0:
        problems.append(f"burst_ratio fell to {result['burst_ratio']} (must stay at or above 4)")
    if problems:
        sys.exit("COST REGRESSION:\n  " + "\n  ".join(problems))
    print("cost within baseline")

if __name__ == "__main__":
    main()
