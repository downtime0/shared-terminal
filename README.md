# agent-terminal

`agent-terminal` opens one visible SSH terminal that a person and a coding
agent can share. The person types into the TUI as usual. A connected agent can
check whether the session is running, read recent terminal output, and send
input through MCP. Sent input and the remote response appear in the same
terminal. Keep the conversation with the agent in Codex; this app is the shared
terminal, not a chat client.

## Requirements

- Linux or another Unix-like system with a working terminal
- Rust and Cargo
- OpenSSH client (`ssh`)
- Codex CLI, if you want to connect the MCP server to Codex

SSH uses your normal SSH config, keys, agent, and host key checks. The app does
not store SSH credentials.

## Install

From this repository, build and install the executable:

```sh
cargo install --path .
```

Cargo installs it as `~/.cargo/bin/agent-terminal`. Make sure `~/.cargo/bin` is
on `PATH`, or use the full path in the commands below. To install the included
man page for your user:

```sh
install -Dm644 man/agent-terminal.1 ~/.local/share/man/man1/agent-terminal.1
man agent-terminal
```

## Start an SSH session

Run the TUI in the foreground in a terminal window. This keeps the shared
session visible to you and available to the MCP tools:

```sh
agent-terminal root@server
```

SSH options go after `--` and are passed directly to `ssh`:

```sh
agent-terminal root@server -- -p 2222 -i ~/.ssh/work_key
```

Type into the terminal normally. `Ctrl+C` sends an interrupt to the remote
process. `Page Up` and `Page Down` scroll the displayed terminal. `F12` exits
the TUI and closes its SSH process. Run one session at a time for your user.

## Connect Codex with MCP

Register the MCP server with Codex:

```sh
codex mcp add agent-terminal -- "$HOME/.cargo/bin/agent-terminal" mcp
```

Restart Codex after registering. Start `agent-terminal user@host` in a visible
terminal, then use Codex in that same user account. The agent and person share
that live SSH session:

1. `session_status` checks that the visible TUI is running.
2. `terminal_read` reads up to 120 recent lines from the rendered terminal.
3. `terminal_send` types the supplied text into the SSH session and presses
   Enter. This runs on the remote host and is visible in the TUI.

The agent can use `terminal_read` to inspect command output, then send follow-up
commands with `terminal_send`. The MCP server cannot create or reconnect an SSH
session; start the visible TUI first. The MCP server only exposes terminal
controls. It does not provide chat or start a separate Codex conversation.

The same tools are available through the local CLI while a TUI session is
running:

```sh
agent-terminal status
agent-terminal read                 # latest 120 lines
agent-terminal read --lines 40
agent-terminal send 'uptime'
```

For full options, run `agent-terminal --help`, `agent-terminal read --help`,
or `agent-terminal send --help`. Codex MCP configuration is shared by the
Codex CLI, desktop app, and IDE extension. See the
[Codex MCP guide](https://developers.openai.com/codex/mcp/) for details.

## Scope

This is a local, single-user bridge between one TUI and one MCP process. It
expects the agent and TUI to run as the same local user on the same machine. It
does not implement SSH itself, manage multiple sessions, or retain a durable
terminal transcript after the TUI exits.
