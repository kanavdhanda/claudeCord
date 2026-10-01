# Spends real tokens (about 100 short Claude Code calls per model). Run from the repo root:
#   python3 scripts/cost/team_benchmark.py --model sonnet --check     fail if claudeCord's cost rose or its saving shrank
#   python3 scripts/cost/team_benchmark.py --model sonnet --update    measure and save a new baseline
"""Multi-agent cost benchmark: one coding task done by a lead and two workers, delivered three ways.

The chat is scripted (examples/replay.rs). Each policy decides what every agent is sent and when. Here every agent is a real
Claude Code session (tools off, forced to reply "ack" so output is the same everywhere), and each input is sent as a real
turn, so the numbers are exactly what the model provider counts. What differs is only delivery, which is what claudeCord controls.

  naive_broadcast  every message to every other agent, one turn each (a plain group chat)
  addressed        each message only to who it names or the lead, one turn each
  claudecord       the real hub core: addressed, coalesced, informing messages ride along

Reported: turns, input tokens, output tokens and cost per policy, and claudeCord as a share of each baseline.
"""
import argparse, json, shutil, subprocess, sys, tempfile, uuid
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
CARGO = shutil.which("cargo") or str(Path.home() / ".cargo" / "bin" / "cargo")
BASELINE = Path(__file__).resolve().parent / "baseline.json"
SEED = "Project context (for later, reply ack): " + " ".join(f"fn handler_{i}(req: Request) -> Response {{ let v = parse(req.body()); validate(&v)?; store.put(v.id, v); Ok(Response::new(200)) }}" for i in range(70))

def claude(model, prompt, extra):
    base = ["claude", "-p", "--model", model, "--output-format", "json", "--tools", "", "--disable-slash-commands",
            "--setting-sources", "", "--append-system-prompt", "Reply with exactly the word ack and nothing else, whatever the message says."]
    j = json.loads(subprocess.run(base + extra + [prompt], capture_output=True, text=True, cwd=tempfile.gettempdir()).stdout)
    u = j["usage"]
    return {"in": u["input_tokens"] + u["cache_creation_input_tokens"] + u["cache_read_input_tokens"], "out": u["output_tokens"], "cost": j["total_cost_usd"]}

def play(model, inputs):
    """One agent: start a session, give it the project context, then send each input as a turn. Counts only the inputs."""
    sid = str(uuid.uuid4())
    claude(model, SEED, ["--session-id", sid])
    prev = claude(model, "warm the cache, reply ack", ["--resume", sid])
    tin = tout = 0
    for text in inputs:
        r = claude(model, text, ["--resume", sid])
        tin += r["in"]
        tout += r["out"]
    return {"turns": len(inputs), "in": tin, "out": tout}

def policy_totals(model, per_agent):
    with ThreadPoolExecutor(max_workers=3) as ex:
        runs = list(ex.map(lambda ins: play(model, ins), per_agent.values()))
    return {"turns": sum(r["turns"] for r in runs), "in": sum(r["in"] for r in runs), "out": sum(r["out"] for r in runs)}

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--model", default="sonnet")
    g = ap.add_mutually_exclusive_group(required=True)
    g.add_argument("--check", action="store_true")
    g.add_argument("--update", action="store_true")
    args = ap.parse_args()
    chat = json.loads(subprocess.run([CARGO, "run", "-q", "--example", "replay"], capture_output=True, text=True, cwd=ROOT, check=True).stdout)
    res = {p: policy_totals(args.model, chat[p]) for p in ("naive_broadcast", "addressed", "claudecord")}
    cc, naive, addr = res["claudecord"], res["naive_broadcast"], res["addressed"]
    res["claudecord_vs_naive"] = round(cc["in"] / naive["in"], 3)
    res["claudecord_vs_addressed"] = round(cc["in"] / addr["in"], 3)
    print(f"{'policy':18}{'turns':>7}{'input tokens':>15}{'output tokens':>15}")
    for p in ("naive_broadcast", "addressed", "claudecord"):
        r = res[p]; print(f"{p:18}{r['turns']:>7}{r['in']:>15,}{r['out']:>15,}")
    print(f"claudecord uses {res['claudecord_vs_naive']:.0%} of the input tokens of a plain group chat and {res['claudecord_vs_addressed']:.0%} of addressed-only")
    key = f"team:{args.model}"
    data = json.loads(BASELINE.read_text()) if BASELINE.exists() else {}
    if args.update:
        data[key] = res
        BASELINE.write_text(json.dumps(data, indent=2) + "\n")
        print(f"saved baseline {key}")
        return
    base = data.get(key)
    if not base:
        sys.exit(f"no baseline {key}; run with --update first")
    problems = []
    if cc["turns"] > base["claudecord"]["turns"]:
        problems.append(f"turns rose from {base['claudecord']['turns']} to {cc['turns']}")
    if cc["in"] > base["claudecord"]["in"] * 1.15:
        problems.append(f"input tokens rose from {base['claudecord']['in']:,} to {cc['in']:,} (limit +15%)")
    if res["claudecord_vs_naive"] > 0.45:
        problems.append(f"claudecord is now {res['claudecord_vs_naive']:.0%} of naive (must stay under 45%)")
    if problems:
        sys.exit("COST REGRESSION:\n  " + "\n  ".join(problems))
    print("cost within baseline")

if __name__ == "__main__":
    main()
