# agent-terminal

A simple shared SSH terminal. The user sees and types in the TUI; an agent can
read the same terminal screen and send commands through MCP. Conversation with
the agent stays in Codex.

## Run

```sh
cargo run -- user@host
```

The app uses your normal `ssh` executable and SSH configuration. Use `Ctrl+C`
to interrupt the remote process, `Page Up`/`Page Down` to scroll, and `F10` to
close the TUI.

Pass SSH options after `--`:

```sh
cargo run -- user@host -- -p 2222 -i ~/.ssh/work_key
```

## Connect Codex with MCP

Install and register the MCP server:

```sh
cargo install --path .
codex mcp add agent-terminal -- "$HOME/.cargo/bin/agent-terminal" mcp
```

Restart Codex after registering. The MCP tools let the agent check the session,
read the visible terminal screen, and send a command to the shared SSH session.
The Codex CLI, desktop app, and IDE extension share MCP configuration. See the
[Codex MCP guide](https://developers.openai.com/codex/mcp/) for details.

The MCP server only exposes terminal controls. It does not contain a chat pane
or create a separate agent conversation.

## Scope

This first version is local and single-session. It expects a Unix-like system,
an installed `ssh`, and one TUI per user. It does not store credentials or
implement SSH itself.
