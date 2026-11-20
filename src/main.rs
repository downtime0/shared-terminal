use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
};
use serde::{Deserialize, Serialize};
use std::{
    env,
    io::{self, BufRead, BufReader, Read, Write},
    num::NonZeroU16,
    path::PathBuf,
    sync::{Arc, Mutex, mpsc},
    thread,
    time::Duration,
};

#[derive(Parser)]
#[command(
    name = "shared-terminal",
    version,
    about = "A shared local or SSH terminal for people and coding agents",
    long_about = "Run one visible terminal session that you and a connected agent can both use. Start a local shell with --local, or connect over SSH with a destination. The person types directly in the terminal; the agent reads its output and sends input through the MCP server.",
    after_help = "Examples:\n  shared-terminal --local\n  shared-terminal root@server\n  shared-terminal root@server -- -p 2222 -i ~/.ssh/id_ed25519\n  shared-terminal read --lines 40\n  shared-terminal send 'systemctl status ssh'\n\nStart the TUI in the foreground before using read, send, or MCP tools. F12 exits the TUI and closes its child process."
)]
struct Cli {
    #[command(subcommand)]
    command: Option<AgentCommand>,
    /// SSH destination, such as user@host
    destination: Option<String>,
    /// Start the user's local interactive shell instead of SSH
    #[arg(long, conflicts_with = "destination")]
    local: bool,
    /// Arguments passed directly to ssh (repeatable)
    #[arg(last = true)]
    ssh_args: Vec<String>,
}

#[derive(Subcommand)]
enum AgentCommand {
    /// Read recent output from the active shared terminal
    Read {
        /// Maximum number of recent lines to return
        #[arg(long, default_value_t = 120)]
        lines: usize,
    },
    /// Send text followed by Enter to the active shared terminal
    Send { text: String },
    /// Check whether the shared terminal TUI is running
    Status,
    /// Run the MCP stdio server registered with Codex
    Mcp,
}

#[derive(Serialize, Deserialize)]
struct Request {
    action: String,
    text: Option<String>,
    lines: Option<usize>,
}

#[derive(Serialize, Deserialize)]
struct Response {
    ok: bool,
    message: String,
}

type SharedWriter = Arc<Mutex<Box<dyn Write + Send>>>;
type Transcript = Arc<Mutex<String>>;

#[derive(Clone, Copy)]
enum PasswordPromptKind {
    Sudo,
    Other,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    if let Some(command) = cli.command {
        if matches!(command, AgentCommand::Mcp) {
            return run_mcp();
        }
        return agent_command(command);
    }
    if cli.local {
        if !cli.ssh_args.is_empty() {
            bail!("SSH arguments cannot be used with --local");
        }
        return run_tui(None, cli.ssh_args);
    }
    let destination = cli.destination.context(
        "missing destination (try: shared-terminal --local or shared-terminal user@host)",
    )?;
    run_tui(Some(destination), cli.ssh_args)
}

fn socket_path() -> PathBuf {
    let uid = env::var("UID").unwrap_or_else(|_| "user".into());
    env::temp_dir().join(format!("shared-terminal-{uid}.sock"))
}

fn agent_command(command: AgentCommand) -> Result<()> {
    use std::os::unix::net::UnixStream;
    let mut stream = UnixStream::connect(socket_path())
        .context("could not connect to shared-terminal; start a session first")?;
    let request = match command {
        AgentCommand::Read { lines } => Request {
            action: "read".into(),
            text: None,
            lines: Some(lines),
        },
        AgentCommand::Send { text } => Request {
            action: "send".into(),
            text: Some(text),
            lines: None,
        },
        AgentCommand::Status => Request {
            action: "status".into(),
            text: None,
            lines: None,
        },
        AgentCommand::Mcp => unreachable!(),
    };
    serde_json::to_writer(&mut stream, &request)?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    let mut response = String::new();
    BufReader::new(stream).read_line(&mut response)?;
    let response: Response = serde_json::from_str(&response)?;
    if !response.ok {
        bail!("{}", response.message);
    }
    println!("{}", response.message);
    Ok(())
}

fn socket_request(request: Request) -> Result<String> {
    use std::os::unix::net::UnixStream;
    let mut stream =
        UnixStream::connect(socket_path()).context("shared-terminal TUI is not running")?;
    serde_json::to_writer(&mut stream, &request)?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    let response: Response = serde_json::from_str(&line)?;
    if !response.ok {
        bail!("{}", response.message);
    }
    Ok(response.message)
}

fn run_mcp() -> Result<()> {
    let stdin = io::stdin();
    let mut stdout = io::BufWriter::new(io::stdout());
    for line in stdin.lock().lines() {
        let line = line?;
        let request: serde_json::Value = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(_) => continue,
        };
        let id = request.get("id").cloned();
        let method = request.get("method").and_then(|m| m.as_str()).unwrap_or("");
        if id.is_none() {
            continue;
        }
        let id = id.as_ref().unwrap();
        let result = match method {
            "initialize" => serde_json::json!({
                "protocolVersion": request.pointer("/params/protocolVersion").and_then(|v| v.as_str()).unwrap_or("2024-11-05"),
                "capabilities": {"tools": {"listChanged": false}},
                "serverInfo": {"name": "shared-terminal", "version": env!("CARGO_PKG_VERSION")}
            }),
            "ping" => serde_json::json!({}),
            "tools/list" => serde_json::json!({"tools": [
                {"name":"terminal_read","description":"Read recent output (up to 120 lines) from the active shared-terminal session. Interactive password prompts are clearly flagged. If one appears, stop and wait for the user; never send a password.","inputSchema":{"type":"object","properties":{}}},
                {"name":"terminal_send","description":"Type the supplied text into the active shared terminal and press Enter. This runs in the selected shell (locally for --local, or remotely for SSH) and is visible to the user in the TUI. If an interactive password prompt is active, input is blocked; stop and wait for the user. Never send a password.","inputSchema":{"type":"object","properties":{"text":{"type":"string","description":"Text to type into the active local shell or SSH session; Enter is appended automatically."}},"required":["text"]}},
                {"name":"session_status","description":"Check whether the user's visible shared-terminal TUI is running and available for shared interaction.","inputSchema":{"type":"object","properties":{}}}
            ]}),
            "tools/call" => {
                let name = request
                    .pointer("/params/name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let args = request
                    .pointer("/params/arguments")
                    .cloned()
                    .unwrap_or(serde_json::json!({}));
                let (action, text) = match name {
                    "terminal_read" => ("read", None),
                    "terminal_send" => (
                        "send",
                        args.get("text").and_then(|v| v.as_str()).map(str::to_owned),
                    ),
                    "session_status" => ("status", None),
                    _ => {
                        write_rpc(
                            &mut stdout,
                            &id,
                            serde_json::json!({"content":[{"type":"text","text":"unknown tool"}],"isError":true}),
                        )?;
                        continue;
                    }
                };
                match socket_request(Request {
                    action: action.into(),
                    text,
                    lines: None,
                }) {
                    Ok(message) => {
                        serde_json::json!({"content":[{"type":"text","text":message}],"isError":false})
                    }
                    Err(error) => {
                        serde_json::json!({"content":[{"type":"text","text":error.to_string()}],"isError":true})
                    }
                }
            }
            _ => {
                write_rpc_error(&mut stdout, &id, -32601, "method not found")?;
                continue;
            }
        };
        write_rpc(&mut stdout, &id, result)?;
    }
    Ok(())
}

fn write_rpc(
    writer: &mut impl Write,
    id: &serde_json::Value,
    result: serde_json::Value,
) -> Result<()> {
    serde_json::to_writer(
        &mut *writer,
        &serde_json::json!({"jsonrpc":"2.0","id":id,"result":result}),
    )?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

fn write_rpc_error(
    writer: &mut impl Write,
    id: &serde_json::Value,
    code: i32,
    message: &str,
) -> Result<()> {
    serde_json::to_writer(
        &mut *writer,
        &serde_json::json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}}),
    )?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

fn run_tui(destination: Option<String>, ssh_args: Vec<String>) -> Result<()> {
    let pty_system = native_pty_system();
    let (width, height) = crossterm::terminal::size()?;
    let (rows, cols) = ssh_dimensions(width, height);
    let pair = pty_system.openpty(PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    })?;
    let (command, mut status) = if let Some(destination) = destination {
        let mut command = CommandBuilder::new("ssh");
        for arg in ssh_args {
            command.arg(arg);
        }
        command.arg(destination.clone());
        (command, format!("SSH: {destination}"))
    } else {
        let shell = env::var("SHELL")
            .ok()
            .filter(|shell| !shell.is_empty())
            .unwrap_or_else(|| "/bin/sh".into());
        let mut command = CommandBuilder::new(&shell);
        command.arg("-i");
        (command, format!("Local shell: {shell}"))
    };
    let mut child = pair
        .slave
        .spawn_command(command)
        .context("failed to start terminal process")?;
    drop(pair.slave);

    let writer: SharedWriter = Arc::new(Mutex::new(pair.master.take_writer()?));
    let transcript: Transcript = Arc::new(Mutex::new(String::new()));
    let (output_tx, output_rx) = mpsc::channel::<Vec<u8>>();
    let mut reader = pair.master.try_clone_reader()?;
    thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if output_tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });

    let path = socket_path();
    let _ = std::fs::remove_file(&path);
    let listener = std::os::unix::net::UnixListener::bind(&path)
        .with_context(|| format!("could not create agent socket at {}", path.display()))?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    let agent_writer = Arc::clone(&writer);
    let agent_transcript = Arc::clone(&transcript);
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let writer = Arc::clone(&agent_writer);
            let transcript = Arc::clone(&agent_transcript);
            thread::spawn(move || {
                let _ = handle_agent(stream, &writer, &transcript);
            });
        }
    });

    let mut terminal = setup_terminal()?;
    let mut parser = vt100::Parser::new(
        NonZeroU16::new(rows).expect("terminal rows are clamped to a nonzero value"),
        NonZeroU16::new(cols).expect("terminal columns are clamped to a nonzero value"),
        2_000,
    );
    let mut scroll = 0usize;
    loop {
        while let Ok(bytes) = output_rx.try_recv() {
            parser.process(&bytes);
            let rendered = parser.screen().contents();
            if let Ok(mut log) = transcript.lock() {
                *log = rendered;
            }
        }
        let screen = transcript.lock().map(|s| s.clone()).unwrap_or_default();
        let password_prompt = password_prompt_line(&screen);
        draw(&mut terminal, &screen, &status, scroll, password_prompt)?;
        if event::poll(Duration::from_millis(40))? {
            match event::read()? {
                Event::Key(key) => {
                    if key.kind == KeyEventKind::Release {
                        continue;
                    }
                    if key.code == KeyCode::Char('c')
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        let _ = writer.lock().map(|mut w| w.write_all(&[3]));
                    } else if key.code == KeyCode::F(12)
                        || (key.code == KeyCode::Char('q')
                            && key.modifiers.contains(KeyModifiers::CONTROL))
                    {
                        break;
                    } else if key.code == KeyCode::PageUp {
                        scroll = scroll.saturating_add(20);
                    } else if key.code == KeyCode::PageDown {
                        scroll = scroll.saturating_sub(20);
                    } else {
                        let bytes = key_to_bytes(key);
                        if !bytes.is_empty() {
                            let _ = writer.lock().map(|mut w| w.write_all(&bytes));
                        }
                    }
                }
                Event::Resize(width, height) => {
                    let (rows, cols) = ssh_dimensions(width, height);
                    pair.master.resize(PtySize {
                        rows,
                        cols,
                        pixel_width: 0,
                        pixel_height: 0,
                    })?;
                    parser.set_size(
                        NonZeroU16::new(rows)
                            .expect("terminal rows are clamped to a nonzero value"),
                        NonZeroU16::new(cols)
                            .expect("terminal columns are clamped to a nonzero value"),
                    );
                }
                _ => {}
            }
        }
        if child.try_wait()?.is_some() {
            status = "Terminal process ended · press F12 to close".into();
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    restore_terminal(&mut terminal)?;
    let _ = std::fs::remove_file(path);
    Ok(())
}

fn handle_agent(
    mut stream: std::os::unix::net::UnixStream,
    writer: &SharedWriter,
    transcript: &Transcript,
) -> Result<()> {
    let mut line = String::new();
    BufReader::new(&stream).read_line(&mut line)?;
    let result = (|| -> Result<String> {
        let request: Request = serde_json::from_str(&line)?;
        match request.action.as_str() {
            "status" => {
                let screen = transcript
                    .lock()
                    .map_err(|_| anyhow::anyhow!("transcript unavailable"))?;
                if let Some((_, kind)) = password_prompt_line(&screen) {
                    Ok(match kind {
                        PasswordPromptKind::Sudo => {
                            "sudo password prompt detected; waiting for the user. Do not send input."
                        }
                        PasswordPromptKind::Other => {
                            "password prompt detected; waiting for the user. Do not send input."
                        }
                    }
                    .into())
                } else {
                    Ok("session is running".into())
                }
            }
            "read" => {
                let log = transcript
                    .lock()
                    .map_err(|_| anyhow::anyhow!("transcript unavailable"))?;
                let count = request.lines.unwrap_or(120);
                let output = log
                    .lines()
                    .rev()
                    .take(count)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect::<Vec<_>>()
                    .join("\n");
                if let Some((_, kind)) = password_prompt_line(&log) {
                    let warning = match kind {
                        PasswordPromptKind::Sudo => "⚠ SUDO PASSWORD REQUIRED",
                        PasswordPromptKind::Other => "⚠ PASSWORD REQUIRED",
                    };
                    Ok(format!(
                        "{warning} — waiting for the user; do not send input.\n\n{output}"
                    ))
                } else {
                    Ok(output)
                }
            }
            "send" => {
                let screen = transcript
                    .lock()
                    .map_err(|_| anyhow::anyhow!("transcript unavailable"))?;
                if password_prompt_line(&screen).is_some() {
                    bail!(
                        "password prompt detected; input blocked. Waiting for the user to respond in the visible terminal."
                    );
                }
                drop(screen);
                let mut text = request.text.unwrap_or_default();
                text.push('\n');
                writer
                    .lock()
                    .map_err(|_| anyhow::anyhow!("terminal unavailable"))?
                    .write_all(text.as_bytes())?;
                Ok("sent to shared terminal".into())
            }
            _ => bail!("unknown action"),
        }
    })();
    let response = match result {
        Ok(message) => Response { ok: true, message },
        Err(error) => Response {
            ok: false,
            message: error.to_string(),
        },
    };
    serde_json::to_writer(&mut stream, &response)?;
    stream.write_all(b"\n")?;
    Ok(())
}

fn key_to_bytes(key: crossterm::event::KeyEvent) -> Vec<u8> {
    match key.code {
        KeyCode::Char(c) if key.modifiers.contains(KeyModifiers::CONTROL) => vec![(c as u8) & 0x1f],
        KeyCode::Char(c) => {
            let mut buf = [0; 4];
            c.encode_utf8(&mut buf).as_bytes().to_vec()
        }
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::Esc => vec![0x1b],
        KeyCode::Up => b"\x1b[A".to_vec(),
        KeyCode::Down => b"\x1b[B".to_vec(),
        KeyCode::Right => b"\x1b[C".to_vec(),
        KeyCode::Left => b"\x1b[D".to_vec(),
        KeyCode::Home => b"\x1b[H".to_vec(),
        KeyCode::End => b"\x1b[F".to_vec(),
        KeyCode::Delete => b"\x1b[3~".to_vec(),
        KeyCode::PageUp => b"\x1b[5~".to_vec(),
        KeyCode::PageDown => b"\x1b[6~".to_vec(),
        _ => Vec::new(),
    }
}

fn draw(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    screen: &str,
    status: &str,
    scroll: usize,
    password_prompt: Option<(usize, PasswordPromptKind)>,
) -> Result<()> {
    terminal.draw(|frame| {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(5), Constraint::Length(1)])
            .split(frame.area());
        let prompt_active = password_prompt.is_some();
        let prompt_style = Style::default()
            .fg(Color::Black)
            .bg(Color::Yellow)
            .add_modifier(Modifier::BOLD);
        let lines: Vec<Line> = screen
            .lines()
            .enumerate()
            .map(|(index, line)| {
                let line = Line::from(line);
                if password_prompt.map(|(line_index, _)| line_index) == Some(index) {
                    line.style(prompt_style)
                } else {
                    line
                }
            })
            .collect();
        let block = if let Some((_, kind)) = password_prompt {
            let title = match kind {
                PasswordPromptKind::Sudo => " ⚠ SUDO PASSWORD REQUIRED · WAITING FOR USER ",
                PasswordPromptKind::Other => " ⚠ PASSWORD REQUIRED · WAITING FOR USER ",
            };
            Block::default()
                .borders(Borders::ALL)
                .border_style(
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                )
                .title(title)
        } else {
            Block::default()
                .borders(Borders::ALL)
                .title(" Shared Terminal · SSH ")
        };
        let mut view = Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false });
        let available = chunks[0].height.saturating_sub(2) as usize;
        let total = screen.lines().count();
        let bottom = total.saturating_sub(scroll);
        let top = bottom.saturating_sub(available);
        view = view.scroll((top.min(u16::MAX as usize) as u16, 0));
        frame.render_widget(view, chunks[0]);
        let footer = if prompt_active {
            Line::from(vec![
                Span::styled(
                    " USER INPUT NEEDED ",
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    " Enter the password in this window; agent input is paused.",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
            ])
        } else {
            Line::from(vec![
                Span::styled(
                    " LIVE ",
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(format!(
                    " {status}  ·  PgUp/PgDn scroll  ·  Ctrl+C interrupt  ·  F12 exit"
                )),
            ])
        };
        frame.render_widget(Paragraph::new(footer), chunks[1]);
    })?;
    Ok(())
}

fn password_prompt_line(screen: &str) -> Option<(usize, PasswordPromptKind)> {
    let (index, line) = screen
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .last()?;
    let line = line.trim().to_ascii_lowercase();
    if line.contains("[sudo] password") || line.contains("sudo password") {
        Some((index, PasswordPromptKind::Sudo))
    } else if line == "password:"
        || line.ends_with("'s password:")
        || (line.starts_with("password for ") && line.ends_with(':'))
        || line.starts_with("enter passphrase for key ")
    {
        Some((index, PasswordPromptKind::Other))
    } else {
        None
    }
}

fn ssh_dimensions(width: u16, height: u16) -> (u16, u16) {
    let cols = width.saturating_sub(2).max(10);
    let rows = height.saturating_sub(3).max(3);
    (rows, cols)
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<io::Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    Terminal::new(CrosstermBackend::new(stdout)).context("failed to initialize TUI")
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}
