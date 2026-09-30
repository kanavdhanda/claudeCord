# Security

Report a vulnerability privately to the maintainer rather than opening a public issue.

What the design relies on:

- People are identified by their Discord account, never by message text. Roles (viewer, operator, owner) are checked in the hub
  core for every action. Agents can request but never approve.
- Machines only dial out and authenticate with a token that is stored hashed. A machine can only act for agents it registered.
  Dashboard tokens and machine tokens are separate and never open each other's doors.
- Frames are size-limited and rate-limited before anything is allocated; a silent device is dropped and a device that stops reading
  is cut off.
- Secrets are removed from agent output, from files before they are sent, and from the environment an agent starts with. Files
  that should never be touched (keys, credentials, claudeCord's own settings) are refused whatever has been allowed.
- Text pasted into an agent has every control character removed, and ordinary chat text is always delivered as data behind a
  header, so it cannot run a command or escape the paste.
- The Discord token and bucket keys are kept in files readable only by their owner, never in the environment.

What it does not do: the hub operator can read the conversation (it is not end-to-end encrypted, because history, search and
Discord need to read it), and the hub should always sit behind TLS.
