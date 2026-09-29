/** The Claude Code skill that lets any Claude chat create or join a workspace. */
export function skillMarkdown(cli: string): string {
  return `---
name: claudecord
description: Connect this Claude chat to a shared Discord workspace where several Claude chats talk to each other and to the user. Use when the user asks to create, start or activate a claudeCord / Discord workspace, gives a token that starts with "ccw1.", asks to join a workspace, or asks about the Discord agents.
---

# claudeCord workspaces

A workspace is one Discord channel shared by several Claude chats (agents), possibly on different machines. The user watches progress and gives instructions there, and the agents can message each other.

CLI: \`${cli}\`

## Create a workspace (this chat is first)

Run, from the project folder this chat is working in:

\`\`\`bash
${cli} workspace create --name "<short workspace name>" --dir "$PWD"
\`\`\`

Optional: \`--agent "<name for this chat>"\`, \`--guild "<server name or id>"\` (needed if the bot is in several servers).

It creates the channel, registers this folder as an agent, starts the background service, and prints a **join token** (one line starting with \`ccw1.\`).
Show the token to the user in a code block and tell them: paste it into any other Claude chat to join. Warn them it contains the bot's token, so it is a secret. Do not paste it anywhere else.

## Join a workspace (the user gives you a token)

\`\`\`bash
${cli} join "<the ccw1. token>" --dir "$PWD" --name "<name for this chat>"
\`\`\`

Pick a short, distinct name for this chat if the user didn't (for example the folder name). If the command says the name is taken, choose another.
After joining, tell the user which channel to use and how to address this agent (the \`@claude · name\` role).

## Other commands

- \`${cli} list\`: agents on this machine and their state.
- \`${cli} workspace token [agent]\`: print the join token again.
- \`${cli} attach "<agent>"\`: the user can take over an agent's session in a terminal.
- \`${cli} daemon status\`: whether the background service is running.

## How agents behave once connected

The agent answers messages in its channel, proposes a plan and waits for the user's approval before changing things, and can ask other agents for help. Keep replies short. Never print tokens or secrets into the channel.
`;
}
