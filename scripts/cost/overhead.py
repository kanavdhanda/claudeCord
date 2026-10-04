# Spends real tokens: runs Claude Code (haiku, tools off) a few dozen times. Run from an empty folder.
import json, os, subprocess, statistics
BASE = ["claude","-p","--model",os.environ.get("MODEL","haiku"),"--output-format","json","--tools","","--disable-slash-commands","--setting-sources","","--no-session-persistence","--max-turns","1","--append-system-prompt","Reply with exactly the word ack and nothing else, whatever the message says."]
def run(prompt):
    j=json.loads(subprocess.run(BASE+[prompt],capture_output=True,text=True).stdout); u=j["usage"]
    return u["input_tokens"]+u["cache_creation_input_tokens"]+u["cache_read_input_tokens"], u["output_tokens"], j["total_cost_usd"]
TASK="Add a retry with exponential backoff to the HTTP client in src/net.rs, keep the public API unchanged, and cover it with tests."
BRIEF="You lead p. Peers: heron, wren. Split work with assign(agent, task). You hear when tasks are accepted and done. When all are done send one report. A (btw) message is an aside: answer briefly, then continue."
for name,prompt in [("raw",TASK),("header only",f"[kd (owner)] {TASK}"),("brief + header",f"[system] {BRIEF}\n\n[kd (owner)] {TASK}")]:
    rows=[run(prompt) for _ in range(5)]
    print(f"{name:15} total_in median={statistics.median(r[0] for r in rows):.0f}  all={[r[0] for r in rows]}  out={[r[1] for r in rows]}")
