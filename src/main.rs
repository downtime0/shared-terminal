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
    path::PathBuf,
    sync::{Arc, Mutex, mpsc},
    thread,
    time::Duration,
};

#[derive(Parser)]
#[command(name = "agent-terminal", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<AgentCommand>,
    /// SSH destination, such as user@host
    destination: Option<String>,
    /// Arguments passed directly to ssh (repeatable)
    #[arg(last = true)]
    ssh_args: Vec<String>,
}

#[derive(Subcommand)]
enum AgentCommand {
    /// Read the current shared session transcript
    Read {
        #[arg(long, default_value_t = 120)]
        lines: usize,
    },
    /// Send text followed by Enter to the shared SSH session
    Send { text: String },
    /// Check whether a shared session is running
    Status,
    /// Read messages from the TUI conversation
    ChatRead,
    /// Post a message in the TUI conversation as the agent
    ChatSay { text: String },
    /// Run this process as an MCP stdio server for Codex
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
type Chat = Arc<Mutex<Vec<ChatMessage>>>;

#[derive(Clone, Serialize, Deserialize)]
struct ChatMessage {
    role: String,
    text: String,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    if let Some(command) = cli.command {
        if matches!(command, AgentCommand::Mcp) {
            return run_mcp();
        }
        return agent_command(command);
    }
    let destination = cli
        .destination
        .context("missing SSH destination (try: agent-terminal user@host)")?;
    run_tui(destination, cli.ssh_args)
}

fn socket_path() -> PathBuf {
    let uid = env::var("UID").unwrap_or_else(|_| "user".into());
    env::temp_dir().join(format!("agent-terminal-{uid}.sock"))
}

fn agent_command(command: AgentCommand) -> Result<()> {
    use std::os::unix::net::UnixStream;
    let mut stream = UnixStream::connect(socket_path())
        .context("could not connect to agent-terminal; start a session first")?;
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
        AgentCommand::ChatRead => Request {
            action: "chat_read".into(),
            text: None,
            lines: None,
        },
        AgentCommand::ChatSay { text } => Request {
            action: "chat_send".into(),
            text: Some(text),
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
        UnixStream::connect(socket_path()).context("agent-terminal TUI is not running")?;
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
                "serverInfo": {"name": "agent-terminal", "version": env!("CARGO_PKG_VERSION")}
            }),
            "ping" => serde_json::json!({}),
            "tools/list" => serde_json::json!({"tools": [
                {"name":"terminal_read","description":"Read the current rendered SSH terminal screen.","inputSchema":{"type":"object","properties":{}}},
                {"name":"terminal_send","description":"Type a command into the shared SSH session and press Enter. This executes remotely.","inputSchema":{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}},
                {"name":"chat_read","description":"Read user and agent messages in the TUI conversation.","inputSchema":{"type":"object","properties":{}}},
                {"name":"chat_send","description":"Post a reply in the TUI conversation so the user sees it there.","inputSchema":{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}},
                {"name":"session_status","description":"Check whether the shared SSH TUI is running.","inputSchema":{"type":"object","properties":{}}}
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
                    "chat_read" => ("chat_read", None),
                    "chat_send" => (
                        "chat_send",
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

fn run_tui(destination: String, ssh_args: Vec<String>) -> Result<()> {
    let pty_system = native_pty_system();
    let pair = pty_system.openpty(PtySize {
        rows: 24,
        cols: 80,
        pixel_width: 0,
        pixel_height: 0,
    })?;
    let mut command = CommandBuilder::new("ssh");
    for arg in ssh_args {
        command.arg(arg);
    }
    command.arg(destination.clone());
    let mut child = pair
        .slave
        .spawn_command(command)
        .context("failed to start ssh")?;
    drop(pair.slave);

    let writer: SharedWriter = Arc::new(Mutex::new(pair.master.take_writer()?));
    let transcript: Transcript = Arc::new(Mutex::new(String::new()));
    let chat: Chat = Arc::new(Mutex::new(Vec::new()));
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
    let agent_chat = Arc::clone(&chat);
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let _ = handle_agent(stream, &agent_writer, &agent_transcript, &agent_chat);
        }
    });

    let mut terminal = setup_terminal()?;
    let mut parser = vt100::Parser::new(24, 80, 2_000);
    let mut status = format!("SSH: {destination}");
    let mut scroll = 0usize;
    let mut chat_mode = false;
    let mut chat_input = String::new();
    loop {
        while let Ok(bytes) = output_rx.try_recv() {
            parser.process(&bytes);
            let rendered = parser.screen().contents();
            if let Ok(mut log) = transcript.lock() {
                *log = rendered;
            }
        }
        let screen = transcript.lock().map(|s| s.clone()).unwrap_or_default();
        let messages = chat.lock().map(|m| m.clone()).unwrap_or_default();
        draw(
            &mut terminal,
            &screen,
            &status,
            scroll,
            &messages,
            &chat_input,
            chat_mode,
        )?;
        if event::poll(Duration::from_millis(40))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Release {
                    continue;
                }
                if key.code == KeyCode::F(2) {
                    chat_mode = !chat_mode;
                } else if chat_mode {
                    match key.code {
                        KeyCode::Esc => {
                            chat_mode = false;
                            chat_input.clear();
                        }
                        KeyCode::Enter => {
                            if !chat_input.trim().is_empty() {
                                if let Ok(mut messages) = chat.lock() {
                                    messages.push(ChatMessage {
                                        role: "user".into(),
                                        text: chat_input.clone(),
                                    });
                                }
                                chat_input.clear();
                            }
                        }
                        KeyCode::Backspace => {
                            chat_input.pop();
                        }
                        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                            chat_input.push(c)
                        }
                        _ => {}
                    }
                } else if key.code == KeyCode::Char('c')
                    && key.modifiers.contains(KeyModifiers::CONTROL)
                {
                    let _ = writer.lock().map(|mut w| w.write_all(&[3]));
                } else if key.code == KeyCode::F(10)
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
        }
        if child.try_wait()?.is_some() {
            status = "SSH process ended · press F10 to close".into();
        }
    }
    restore_terminal(&mut terminal)?;
    let _ = std::fs::remove_file(path);
    Ok(())
}

fn handle_agent(
    mut stream: std::os::unix::net::UnixStream,
    writer: &SharedWriter,
    transcript: &Transcript,
    chat: &Chat,
) -> Result<()> {
    let mut line = String::new();
    BufReader::new(&stream).read_line(&mut line)?;
    let result = (|| -> Result<String> {
        let request: Request = serde_json::from_str(&line)?;
        match request.action.as_str() {
            "status" => Ok("session is running".into()),
            "read" => {
                let log = transcript
                    .lock()
                    .map_err(|_| anyhow::anyhow!("transcript unavailable"))?;
                let count = request.lines.unwrap_or(120);
                Ok(log
                    .lines()
                    .rev()
                    .take(count)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect::<Vec<_>>()
                    .join("\n"))
            }
            "send" => {
                let mut text = request.text.unwrap_or_default();
                text.push('\n');
                writer
                    .lock()
                    .map_err(|_| anyhow::anyhow!("terminal unavailable"))?
                    .write_all(text.as_bytes())?;
                Ok("sent to shared SSH session".into())
            }
            "chat_read" => {
                let messages = chat
                    .lock()
                    .map_err(|_| anyhow::anyhow!("chat unavailable"))?;
                Ok(serde_json::to_string(&*messages)?)
            }
            "chat_send" => {
                chat.lock()
                    .map_err(|_| anyhow::anyhow!("chat unavailable"))?
                    .push(ChatMessage {
                        role: "agent".into(),
                        text: request.text.unwrap_or_default(),
                    });
                Ok("message shown in TUI".into())
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
    messages: &[ChatMessage],
    chat_input: &str,
    chat_mode: bool,
) -> Result<()> {
    terminal.draw(|frame| {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Percentage(68),
                Constraint::Min(5),
                Constraint::Length(1),
            ])
            .split(frame.area());
        let lines: Vec<Line> = screen.lines().map(Line::from).collect();
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" Agent Terminal · shared SSH ");
        let mut view = Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false });
        let available = chunks[0].height.saturating_sub(2) as usize;
        let total = screen.lines().count();
        let bottom = total.saturating_sub(scroll);
        let top = bottom.saturating_sub(available);
        view = view.scroll((top.min(u16::MAX as usize) as u16, 0));
        frame.render_widget(view, chunks[0]);
        let chat_lines: Vec<Line> = messages
            .iter()
            .rev()
            .take(chunks[1].height.saturating_sub(3) as usize)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .map(|m| {
                let (label, color) = if m.role == "agent" {
                    ("AGENT", Color::Cyan)
                } else {
                    ("YOU", Color::Yellow)
                };
                Line::from(vec![
                    Span::styled(
                        format!("{label}  "),
                        Style::default().fg(color).add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(m.text.clone()),
                ])
            })
            .collect();
        let chat_title = if chat_mode {
            " Conversation · typing here · Enter sends · Esc returns "
        } else {
            " Conversation · F2 to message agent "
        };
        let chat_block = Block::default().borders(Borders::ALL).title(chat_title);
        let chat_content = Paragraph::new(chat_lines)
            .block(chat_block)
            .wrap(Wrap { trim: false });
        frame.render_widget(chat_content, chunks[1]);
        if chat_mode && chunks[1].height > 2 {
            let input_y = chunks[1].y + chunks[1].height - 2;
            frame.render_widget(
                Paragraph::new(format!("> {chat_input}")),
                ratatui::layout::Rect::new(
                    chunks[1].x + 1,
                    input_y,
                    chunks[1].width.saturating_sub(2),
                    1,
                ),
            );
            frame.set_cursor_position((
                chunks[1].x + 3 + chat_input.chars().count() as u16,
                input_y,
            ));
        }
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    " LIVE ",
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(format!(
                    " {status}  ·  F2 chat  ·  PgUp/PgDn scroll  ·  F10 quit"
                )),
            ])),
            chunks[1],
        );
    })?;
    Ok(())
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
