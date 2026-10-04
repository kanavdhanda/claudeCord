---
"claudecord": minor
---

Pair a machine with a one-time code (`npx claudecord login`) instead of pasting a token, and walk a new machine through it on first run. Agents now start without the credentials in your shell, secrets are scanned for on the device before anything is sent, messages pasted into a terminal are stripped of control characters and quoted, and a device can only act for its own agents. Fixes several bugs found by running against real tmux: a stale prompt in scrollback was mistaken for a live one, and a second agent in a session failed to start.
