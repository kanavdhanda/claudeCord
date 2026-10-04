# claudeCord

Run coding agents on any machines and manage them from a group chat.

You put Claude Code, Codex or agy to work in a folder. Each agent shows up in a Discord channel under its own name, talks to
the other agents like a teammate, and asks you when it is stuck. You answer in Discord. Nothing about it depends on which agent
program is behind a name.

## How it works, in plain words

- A **hub** is one small program on a server you control. It knows who is on the team, who may do what, and where every message
  goes. It keeps a record of the conversation.
- A **daemon** runs on each machine that has agents. It starts the agents in terminals it owns, types messages into them at a
  safe moment (never over a half-typed line), and tells the hub what they are doing. Machines only ever dial out, so no ports
  are opened and it works behind a home router, a company firewall or a web proxy.
- **Discord** is where people watch and steer. Each project is a channel, each task a thread, each agent a name. Questions and
  permission requests arrive with buttons.
- Agents talk back by running short shell commands (`claudecord say`, `ask`, `done`, ...). There are no tools to load into the
  agent, so they cost almost nothing.

```
people  <->  Discord  <->  hub  <->  machine daemons  <->  agents in terminals
                            |
                  history, roles, saved state (SQLite) and old history in a bucket
```

## Try it (about ten minutes)

Install the program with `pip install claudecord` or `npm install -g claudecord` (no compiler needed; Linux, macOS and Windows), or build it
with `cargo build --release`. You also need a Discord server you own.

**1. Make the Discord bot.** In the Discord developer portal create an application and a bot, copy the bot token, and turn on the
Message Content intent. Then on the hub's machine:

    claudecord discord set --guild YOUR_SERVER_ID --token-file bot-token.txt
    claudecord discord invite            # open the link it prints and add the bot to your server

**2. Start the hub.**

    claudecord hub --data claudecord-hub --bind 127.0.0.1:8787

The bot's owner in Discord becomes the owner of the team. For anything beyond one machine, put a TLS proxy (Caddy, nginx,
Cloudflare) in front and bind to 127.0.0.1; the hub refuses a public address without TLS unless you insist.

**3. Give a machine a token and log it in.**

    claudecord token mac --data claudecord-hub          # prints a token once
    claudecord login --hub wss://your-hub.example.com --token THE_TOKEN --name mac
    claudecord doctor                                   # checks the route step by step

**4. Start an agent in a project folder.**

    cd ~/code/myproject
    claudecord start                                    # starts claude here in its own tmux session and opens it

A channel named after the folder appears in Discord. Type there and the agent hears you. Leave with the usual tmux detach
(Ctrl-b d); the agent keeps running. `claudecord attach NAME` returns to it. Where tmux is not installed (native Windows) a
built-in terminal is used instead, and Ctrl-] leaves it.

`claudecord start` does exactly what you ask and nothing more. Every extra is a flag you choose:

| Flag | Does |
|---|---|
| `--worktree` | gives the agent its own git worktree and branch (without it, a second agent in a busy folder is refused) |
| `--pickup` | hands over the state the previous session saved |
| `--restart N` | starts the agent again up to N times if it dies (default: never) |
| `-- COMMAND...` | runs that program instead of the adapter's default |

Start-up dialogs (such as a trust question) are never answered for you: they reach the chat as a permission request.

## Agents cannot act as each other

Each agent is started with its own secret key and works in its own folder. Its commands (`say`, `ask`, `done`, ...) are
checked against that key by the daemon, so an agent cannot speak, ask or finish a task as another agent. This stops mistakes
and confusion, not a hostile program running as the same user: such a program could read another agent's folder, and agents
share one private tmux server, so an agent that runs `tmux` itself can reach other agents' sessions.

## Logs and handoff

Everything an agent does is written to a private log with secrets removed: an event log (messages delivered, said, asked,
decisions, start, end) and a copy of its terminal.

    claudecord logs otter                 # recent events
    claudecord logs otter --terminal      # recent terminal text, control codes removed
    claudecord handoff otter              # writes a markdown note of what happened, for the next session or person

## Who can do what

People are known by their Discord account, never by what a message says. Each project has three roles:

| Role | May |
|---|---|
| viewer | read everything |
| operator | instruct agents, answer questions, allow normal actions once, stop or pause an agent |
| owner | everything, including standing permissions, roles, stopping everything, starting agents, raw terminal input |

Anyone not on the list is ignored. Agents can ask and request but never approve anything. Messages from bots and webhooks (which
includes every agent's own posts) are never treated as a person.

## Slash commands in Discord

`/agents` `/devices` `/status` `/pause` `/resume` `/stop` `/killall` `/btw` `/grant` `/revoke` `/role` `/dump` `/pickup` `/raw`
`/spawn`. Roles are checked by the hub for each one.

## Permissions

When an agent wants to do something that needs a person, a message appears with buttons: Deny, Allow once, Allow this kind, Allow
all. Normal actions can be allowed by an operator; risky ones (leaving the project, installing, pushing, unknown hosts) need an
owner. "This kind" and "all" are standing permissions: only an owner can give them, and every one expires (an hour by default;
`/grant` sets another time). Files that should never be touched (keys, credentials, claudeCord's own settings) are refused
without asking, whatever has been allowed. Answering at the terminal instead also works, and the Discord message is closed.

## Slash and at-sign in messages

Agent programs treat some characters specially (a leading `/` for commands, `@` for files or plugins). claudeCord stays out of
that: **every ordinary message is delivered as data behind a header** such as `[kd (owner)] /clear`, so the first character the
agent program sees is never `/` or `@`, and nothing a person types in chat can run a command by accident. To send a command on
purpose, an owner uses `/raw agent:otter text:/compact`, which types exactly that text with no header. The hub knows nothing
about what any command means, so it works the same for every agent program. Every use is recorded.

## Dashboard

The hub also serves a read-only page that shows machines (and whether they are connected), agents and what each is doing,
what is waiting on a person, tasks, and the conversation. Make a token and open the hub's address in a browser:

    claudecord web-token me --data claudecord-hub      # prints a token once; paste it into the page

A dashboard token opens the page and nothing else, and a machine's token never opens it. The page draws everything as text and
is served with a strict content policy.

## What is guaranteed about messages

Nothing is acknowledged before it is on disk. Each batch of changes (the history rows and what changed in the saved state) is written to the
database as one transaction, and only then are frames sent, chat posts made, reactions added or callers answered. So a message that was
acknowledged survives a crash, and the saved state and the history always agree. This is tested by killing the real hub with SIGKILL at random
moments while messages flow.

- **Machine to hub:** every frame is numbered, acknowledged only once saved, kept by the machine until then (even while the hub is away) and sent
  again after a reconnect. The hub takes each numbered frame once, also across its own restarts.
- **Discord to hub:** the hub remembers the newest message taken from each channel, saved in the same commit as the message, so a message read
  twice (a resumed connection, a restart) is taken once. After a fresh connection the bridge reads back what was said while it was away, oldest first.
- **Hub to agent:** waiting messages are part of the saved state and are delivered again after a restart; the machine drops a message it already
  has by its id (at-least-once, effectively once).

What is not covered: if the machine's own program restarts, frames it had not yet had acknowledged are lost with it; answers, permission decisions
and files the hub sends to a machine that is away are not queued for it; a post to Discord can be lost if the hub is killed between saving a message
and posting it (the history has it); and a disk that fails leaves the hub carrying on without durability, which it logs loudly.

## Logs and what survives what

The hub writes one line per event to the terminal and to `hub.log` in its data folder (kept to about 20 MB in two files; read it with
`tail -f`, or `journalctl -u claudecord-hub` under systemd). The machine daemon logs to `daemon.log` in `~/.claudecord`, and each agent has
its own event and terminal logs (`claudecord logs`). Set `CLAUDECORD_LOG=debug` for more, or `error` for less. Every line is scrubbed of
secrets, and a panic anywhere is logged with where it happened.

What each part does when something goes wrong: a bug while handling one device's frame, one command or one terminal costs only that step,
is logged, and the hub or daemon carries on; the Discord bridge restarts itself if it panics, and keeps retrying if Discord is down at start-up;
a bug caught inside the hub is reported to the affected projects' chats (owner pinged) before anything else; each agent on a machine is looked at on its own, so one agent's fault is reported and the others carry on, and one that keeps failing is stopped and reported; a device that misbehaves or stops reading is cut off with the reason in the log, and nobody else notices; a normal stop (Ctrl-C, or SIGTERM from
`systemctl stop` or Docker) saves everything and is recorded as a stop, while a kill is counted as downtime from the last heartbeat.

The log can also be read over HTTP, `GET /api/v1/logs?lines=200&level=warn&q=text` (a dashboard token or a signed-in workspace owner; the dashboard
shows it as a panel for them), and the hub is set up for systemd to restart it if its core hangs, not only if it dies (`deploy/claudecord-hub.service`:
never gives up restarting, `Type=notify` with a watchdog); the Docker image has a health check. Database backups to the bucket run hourly.

Failures are also prevented where they can be: the hub checks before serving that its data folder is writable and its database is sound
(and says what to do if not), explains a port already in use, warns if the process may open too few files, refuses devices past a limit
instead of running out of file handles, caps agents per project, and logs when its core stops answering. A machine refuses an agent whose
program is not installed or whose folder is gone, immediately and with the reason.

## Is it up?

The hub records its own state and the Discord bridge's (a crash counts as down from the last heartbeat), answers `/healthz` (the
process is there) and `/readyz` (it can do its job, including Discord), and serves Prometheus text at `/metrics`.

    claudecord uptime --target 99.9       # availability over 1 hour, 1 day, 1 week, 1 month, and the error budget left
    claudecord probe https://hub.example.com --name eu    # on ANOTHER machine: an outside check with its own record

A scheduled GitHub Actions check (`.github/workflows/uptime.yml`, set the repository variable `HUB_URL`) is the free safety net.
`deploy/` has a Caddyfile (automatic TLS), a systemd unit, and `bootstrap.sh`, which sets up a hub on a fresh Ubuntu or Debian machine in one command (not yet tried on a real server).

## Keeping cost down

For an agent, the cost of a message is the turn it causes, because every turn rereads the whole conversation. So:

- everything waiting for an agent is delivered as **one** input;
- a plain `say` between agents is information and waits to ride along with the next message that needs a reply (use `@name` or
  `ask` to need an answer);
- nothing is broadcast: a message goes to who it names, otherwise to the lead;
- permissions, mirroring to Discord, history and files cost the agent nothing; a file is one short line, however big.

Measured on real Claude Code with a scripted lead-and-two-workers task: **9 turns instead of 32**, about **28% of a plain group
chat's input tokens** on Sonnet. Run `scripts/cost/team_benchmark.py` (it spends tokens) to repeat it.

## When a session runs out

At 97% of the session allowance (or an agent's context), every affected agent is told to save its state with `claudecord dump`.
A fresh session asks for it with `claudecord pickup` and carries on from the saved state, not from a replay of the old
conversation. Finished work is never handed over again.

## History, old files and Obsidian

Recent history is hot and old history is cold, the way large chat systems keep it: the newest messages live in the database; older days
move out into immutable compressed files, one per project per day (a huge day is split so rolling over never needs much memory), which are
joined if a day ends up with several and, with a bucket set up, uploaded and then removed from the disk. Reading goes the other way: the
dashboard's "Load older" button pages back through the database and then the files, newest first, keeping the last few files unpacked. So the
database stays small and fast however many messages there are.

The last two weeks of conversation stay in the hub's database. Older history is compressed into files and moved to an
S3-compatible bucket (Oracle Cloud's free tier first, Cloudflare R2 later), so a small free server never fills its disk:

    claudecord storage oracle --namespace NS --region us-ashburn-1 --bucket claudecord-history --key-id KEY --secret-file secret.txt
    claudecord storage test
    claudecord storage move r2.json         # later: copy everything to another provider, verify, switch
    claudecord storage backup               # the hub also copies the live database to the bucket every few hours
    claudecord storage restore              # on a host that lost its disk: bring the database back

To read and graph the whole conversation in Obsidian (messages, plans, questions and answers, permission decisions, tasks and the agents' reports),
open a folder as a vault and let the hub keep it up to date while it runs, and once more when it stops:

    claudecord hub --data claudecord-hub --vault ~/Vault/claudeCord          # refreshed every 30 s (--vault-every)
    claudecord export --data claudecord-hub --out ~/Vault/claudeCord         # or write it once, or with --watch 30

It writes plain Markdown notes with links: a note per day and thread, one per person or agent, one per task, one per question (who asked, what, who
answered, the answer), one per permission request (what, and who decided) and one per report (title, summary and the files it named). Each project
has an index page that links them all. It includes history that has already moved into files or the bucket, and running it again changes nothing.
Use Obsidian's graph view to see who talked to whom.

## Several machines

Each machine reports its size, how many agents it should run and labels such as `gpu`. `/spawn` places a new agent on the least
loaded machine with room. A machine refuses agents beyond its limit, and a second agent in the same git folder is given its own
worktree and branch so two agents never edit the same files. Machines that are asleep reconnect the moment they wake.

## Safety

- Secrets are removed from anything an agent posts, from files before they are sent, and from the environment an agent starts with.
- Tokens are stored only as hashes. Bucket keys and the Discord token live in files only you can read.
- A device can only act for agents it registered. Frames are size-limited and rate-limited; a device that goes quiet or stops
  reading is dropped.
- Text pasted into an agent has every control character removed, so it cannot escape the paste.
- The hub survives a bug in one handler by rebuilding from its last save, and restarts lose nothing it had acknowledged.

## Checking that it works

    scripts/check.sh           # format, lints, every test, then a live check of every feature, then the shipped binary
    scripts/check.sh --fix     # repairs formatting, simple lints and the code map, then checks
    claudecord selftest        # starts the real pieces and proves each feature is alive

`CODEMAP.md` lists every file and what it is, generated from each file's own header. CI runs the same checks on Linux (x86 and ARM)
and macOS.

## Honest limits

- The Discord side is tested against a stand-in Discord, not against the real service yet.
- Typing into Claude Code, Codex and agy is done through the terminal. The screen-reading rules for agy and Codex are generic and
  unverified against the live programs; whether a pasted `/command` runs the same way in every program is also unverified.
- The dashboard is read-only. Sign in with Discord (`claudecord discord oauth`) shows each person only the projects they have a
  role in; it is tested against a stand-in Discord, not the real one. A dashboard token still shows everything.
- A load test exists (`scripts/load/`, k6, users with three bots each); it was run only up to about 9,000 users on a laptop, where k6
  itself ran out of threads. The 10,000-user runs on free-server sizes are a CI job (`load.yml`) that has not been run yet.
- The pip and npm packages are built and tested by `scripts/test_packaging.py`, but nothing has been published yet.
- Windows is built in CI but has not been run by me. Use tmux inside WSL for the tested route; native Windows uses the
  built-in ConPTY terminal, and the stand-in-agent tests skip themselves there.
- tmux cannot see a half-typed line, only that someone was recently active, so messages wait for quiet instead.
- Only the generic terminal driver exists. Structured drivers for Claude Code hooks, ACP and the Codex app server are not built.
