# Spends real tokens: runs Claude Code (tools off) about a dozen times. Run from an empty folder. MODEL=sonnet to change model.
"""Five follow-up messages as five turns versus one coalesced turn, in a real Claude Code session.
total_cost_usd from the CLI is cumulative for a resumed session, so per-turn cost is the difference between turns."""
import json, os, subprocess, uuid

BASE = ["claude", "-p", "--model", os.environ.get("MODEL", "haiku"), "--output-format", "json", "--tools", "",
        "--disable-slash-commands", "--setting-sources", "",
        "--append-system-prompt", "Acknowledge every message with exactly the word ack and nothing else."]

def run(prompt, extra):
    j = json.loads(subprocess.run(BASE + extra + [prompt], capture_output=True, text=True).stdout)
    u = j["usage"]
    return dict(total_in=u["input_tokens"] + u["cache_creation_input_tokens"] + u["cache_read_input_tokens"],
                cached=u["cache_read_input_tokens"], out=u["output_tokens"], cost=j["total_cost_usd"])

SEED = " ".join(f"fn handler_{i}(req: Request) -> Response {{ let v = parse(req.body()); validate(&v)?; store.put(v.id, v); Ok(Response::new(200)) }}" for i in range(int(os.environ.get("SEED_FNS", "70"))))
MSGS = ["also log each retry at debug level", "use the tracing crate", "no new dependencies please", "keep functions short", "thanks"]

def seeded():
    sid = str(uuid.uuid4())
    r = run(f"Context for later (do not summarise, reply ack):\n{SEED}", ["--session-id", sid])
    run("warm the cache, reply ack", ["--resume", sid])  # so the measured turns all hit a warm cache
    return sid, r

sidA, a = seeded()
sidB, b = seeded()
print(f"model={os.environ.get('MODEL','haiku')} seeded context ~{a['total_in']} input tokens")
prev = run("again, reply ack", ["--resume", sidA])
sep = []
for m in MSGS:
    r = run(f"[kd (owner)] {m}", ["--resume", sidA]); sep.append(dict(r, delta=r["cost"] - prev["cost"])); prev = r
prevb = run("again, reply ack", ["--resume", sidB])
r = run("\n".join(f"[kd (owner)] {m}" for m in MSGS), ["--resume", sidB]); one = dict(r, delta=r["cost"] - prevb["cost"])
tot_sep = sum(x["delta"] for x in sep)
print(f"5 turns: input={sum(x['total_in'] for x in sep)} cached={sum(x['cached'] for x in sep)} out={sum(x['out'] for x in sep)} cost=${tot_sep:.4f}")
print(f"1 turn : input={one['total_in']} cached={one['cached']} out={one['out']} cost=${one['delta']:.4f}")
print(f"ratio  : input x{sum(x['total_in'] for x in sep)/one['total_in']:.1f}, cost x{tot_sep/max(one['delta'],1e-9):.1f}")
