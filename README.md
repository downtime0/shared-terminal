# agent-terminal

A shared SSH workspace with one live terminal and a separate conversation pane.
You can type remote shell input directly, while an agent can read the same
terminal, send commands, and reply in the conversation pane through MCP.

## Run

```sh
cargo run -- user@host
```

The app uses your normal `ssh` executable and SSH configuration. Press `F2` to
focus the conversation, type a message, and press `Enter` to send it. Press
`Esc` to return to the SSH terminal. Press `Ctrl+C` in terminal mode to interrupt
the remote process, `Page Up`/`Page Down` to scroll, and `F10` to close the TUI.

Pass SSH options after `--`:

```sh
cargo run -- user@host -- -p 2222 -i ~/.ssh/work_key
```

## Connect Codex with MCP

Install the binary and register its MCP server with Codex:

```sh
cargo install --path .
codex mcp add agent-terminal -- "$HOME/.cargo/bin/agent-terminal" mcp
```

Restart Codex after adding the MCP server. Its tools include:

- `chat_read`, `chat_send`, and `chat_wait` for the conversation pane
- `terminal_read` and `terminal_send` for the shared SSH session
- `session_status` to check the TUI connection

To use the conversation pane, keep a Codex task open with the `agent-terminal`
MCP available. Ask the agent to wait for messages with `chat_wait`; messages
typed into the TUI then wake that call and the agent can respond with
`chat_send`. MCP gives Codex access to the app, but it does not start an agent
or make an idle Codex task wake on its own.

Codex stores MCP server entries in `~/.codex/config.toml`; the CLI command above
adds a local stdio server entry there. The Codex CLI, desktop app, and IDE
extension share this MCP configuration. See the [Codex MCP guide](https://developers.openai.com/codex/mcp/) for configuration details.

## Scope

This first version is local and single-session. It expects a Unix-like system,
an installed `ssh`, and one TUI per user. It does not store credentials or
implement SSH itself. Conversation messages live in memory and clear when the
TUI exits.
