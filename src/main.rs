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
    let (width, height) = crossterm::terminal::size()?;
    let (rows, cols) = ssh_dimensions(width, height);
    let pair = pty_system.openpty(PtySize {
        rows,
        cols,
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
    let mut parser = vt100::Parser::new(rows, cols, 2_000);
    let mut status = format!("SSH: {destination}");
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
        draw(&mut terminal, &screen, &status, scroll)?;
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
                Event::Resize(width, height) => {
                    let (rows, cols) = ssh_dimensions(width, height);
                    pair.master.resize(PtySize {
                        rows,
                        cols,
                        pixel_width: 0,
                        pixel_height: 0,
                    })?;
                    parser.set_size(rows, cols);
                }
                _ => {}
            }
        }
        if child.try_wait()?.is_some() {
            status = "SSH process ended · press F10 to close".into();
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
) -> Result<()> {
    terminal.draw(|frame| {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(5), Constraint::Length(1)])
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
                    " {status}  ·  PgUp/PgDn scroll  ·  Ctrl+C interrupt  ·  F10 exit"
                )),
            ])),
            chunks[1],
        );
    })?;
    Ok(())
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
