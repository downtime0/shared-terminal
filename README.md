# shared-terminal

`shared-terminal` opens one visible terminal session that a person and a coding
agent can share. Run a local shell or connect to a remote host over SSH. The
person types into the TUI as usual. A connected agent can check whether the
session is running, read recent terminal output, and send input through MCP.
Keep the conversation with the agent in Codex; this app is the shared terminal,
not a chat client.


## Scope

This is a local, single-user bridge between one TUI and one MCP process. It
expects the agent and TUI to run as the same local user on the same machine. It
does not implement SSH itself, manage multiple sessions, or retain a durable
terminal transcript after the TUI exits.

## Expected Workflow (after installation)

1. Open two terminals.
2. In first terminal, start `shared-terminal --local` or `shared-terminal root@192.168.1.190` (maybe snap it to the right side of your screen)
    - if starting a ssh session, you may be propted for you password next
3. In second terminal, start your agent (i.e. `codex` or `claud`, etc...) (maybe snap it to the left side of the screen)
    - tell your agent to connect to the shared-terminal mcp (the mcp should have previously been registered during install)
    - once connected, give your agent a prompt for the shared terminal (i.e. `check the disk health`)

I hope you get the point. Use this for visiblity into agents working in your ssh or local environments.

## Requirements

- Linux or another Unix-like system with a working terminal
- Rust and Cargo
- OpenSSH client (`ssh`) for remote sessions
- Codex CLI, if you want to connect the MCP server to Codex

SSH sessions use your normal SSH config, keys, agent, and host key checks. The
app does not store SSH credentials.

## Install

From this repository, build and install the executable:

```sh
cargo install --path .
```

Cargo installs it as `~/.cargo/bin/shared-terminal`. Add Cargo's bin directory
to `PATH` so you can run `shared-terminal` directly:

```sh
echo 'export PATH="$HOME/.cargo/bin:$PATH"' >> ~/.bashrc
source ~/.bashrc
```

If you use a shell other than Bash, add `~/.cargo/bin` to that shell's startup
file instead. To install the included man page for your user:

```sh
install -Dm644 man/shared-terminal.1 ~/.local/share/man/man1/shared-terminal.1
man shared-terminal
```

## Connect Codex with MCP

Register the MCP server with Codex:

```sh
codex mcp add shared-terminal -- "$HOME/.cargo/bin/shared-terminal" mcp
```

Restart Codex after registering. Start `shared-terminal --local` or
`shared-terminal user@host` in a visible terminal, then use Codex in that same
user account. The agent and person share that live session:

1. `session_status` checks that the visible TUI is running.
2. `terminal_read` reads up to 120 recent lines from the rendered terminal.
3. `terminal_send` types the supplied text into the session and presses Enter.
   It runs locally in local mode or on the remote host in SSH mode, and is
   visible in the TUI.

The agent can use `terminal_read` to inspect command output, then send follow-up
commands with `terminal_send`. The MCP server cannot create or reconnect an SSH
session; start the visible TUI first. The MCP server only exposes terminal
controls. It does not provide chat or start a separate Codex conversation.

## Start a session

Start a shared local shell:

```sh
shared-terminal --local
```

The app runs your configured `$SHELL` interactively inside the TUI. You and the
agent share that local shell, so commands sent by the agent run on your machine
as your user.

To connect to a remote host, pass an SSH destination. Run the TUI in the
foreground in a terminal window to keep the session visible and available to
the MCP tools:

```sh
shared-terminal root@server
```

SSH options go after `--` and are passed directly to `ssh`:

```sh
shared-terminal root@server -- -p 2222 -i ~/.ssh/work_key
```

Type into the terminal normally. `Ctrl+C` sends an interrupt to the active
process. `Page Up` and `Page Down` scroll the displayed terminal. `F12` exits
the TUI and closes its shell or SSH process. Run one session at a time for your
user.

## Security and secrets

MCP can read recent rendered terminal output and send text to the active shell.
Commands run with the permissions of that shell: your local user in local mode,
or the connected account in SSH mode. An agent can therefore read files and
environment variables that account can access by running shell commands.

Do not paste tokens, passwords, or other secrets into commands or other
terminal input that may be echoed. If a secret appears in rendered output, MCP
can read it. Standard no-echo password prompts normally do not display the
typed password, but avoid relying on that as a way to share secrets. The app
does not provide a separate token or password store.

The same tools are available through the local CLI while a TUI session is
running:

```sh
shared-terminal status
shared-terminal read                 # latest 120 lines
shared-terminal read --lines 40
shared-terminal send 'uptime'
```

For full options, run `shared-terminal --help`, `shared-terminal read --help`,
or `shared-terminal send --help`. Codex MCP configuration is shared by the
Codex CLI, desktop app, and IDE extension. See the
[Codex MCP guide](https://developers.openai.com/codex/mcp/) for details.

