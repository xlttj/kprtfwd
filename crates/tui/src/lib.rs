//! The kprtfwd terminal UI: one list of all forwards across all contexts.
//!
//! It asks the daemon for a snapshot every 300 ms; the local socket makes that
//! cheap, and it keeps the UI free of a push protocol.

mod app;
mod view;

use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result};
use kprtfwd_core::{Config, paths};
use kprtfwd_proto::{Client, Request, Response, connect_or_start};
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::widgets::Clear;
use tokio::sync::mpsc;

use crate::app::{App, Cmd, Msg};

const REFRESH: Duration = Duration::from_millis(300);

pub async fn run() -> Result<()> {
    // A broken config.toml is reported now, not only when quitting.
    let config_path = paths::config_path().context("neither KPRTFWD_HOME nor HOME is set")?;
    let config = Config::load(&config_path)?;
    let mut client = attach().await?;
    let snapshot = client.snapshot().await?;
    let mut app = App::new(snapshot);
    app.keep_forwards_running = config.keep_forwards_running;
    app.config_path = home_relative(&config_path);

    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, client, app).await;
    ratatui::restore();
    result
}

/// Connects to the daemon as a TUI, starting the daemon if needed. When the
/// last TUI's connection closes, the daemon switches the forwards off unless
/// the settings say to keep them running.
async fn attach() -> Result<Client> {
    let socket = paths::usable_socket_path().map_err(anyhow::Error::msg)?;
    let exe = std::env::current_exe().context("find the kprtfwd binary")?;
    let mut client = connect_or_start(&socket, &exe).await?;
    client.act(Request::Attach).await?;
    Ok(client)
}

/// `path` with the home directory written as `~`.
fn home_relative(path: &std::path::Path) -> String {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .and_then(|home| path.strip_prefix(home).ok())
        .map_or_else(
            || path.display().to_string(),
            |rest| format!("~/{}", rest.display()),
        )
}

async fn event_loop(
    terminal: &mut DefaultTerminal,
    mut client: Client,
    mut app: App,
) -> Result<()> {
    let (tx, mut rx) = mpsc::unbounded_channel::<Msg>();
    // Terminal input blocks, so it gets its own thread.
    let keys = tx.clone();
    std::thread::spawn(move || {
        while let Ok(event) = event::read() {
            let msg = match event {
                Event::Key(key) if key.kind == KeyEventKind::Press => Msg::Key(key),
                // Anything else, like a resize, just redraws.
                _ => Msg::Done(Ok(String::new())),
            };
            if keys.send(msg).is_err() {
                return;
            }
        }
    });
    let mut refresh = tokio::time::interval(REFRESH);

    loop {
        terminal.draw(|frame| view::draw(frame, &app))?;
        let cmds = tokio::select! {
            Some(msg) = rx.recv() => {
                // Resizes arrive as an empty Done; they must not wipe the message.
                if matches!(&msg, Msg::Done(Ok(text)) if text.is_empty()) {
                    continue;
                }
                // Ctrl+L redraws the whole screen, as in most terminal programs.
                if let Msg::Key(key) = &msg
                    && key.code == KeyCode::Char('l')
                    && key.modifiers.contains(KeyModifiers::CONTROL)
                {
                    // A blank frame first, so the next one rewrites every
                    // cell. Unlike Terminal::clear it needs no answer from
                    // the terminal.
                    terminal.draw(|frame| frame.render_widget(Clear, frame.area()))?;
                    continue;
                }
                app.update(msg)
            }
            _ = refresh.tick() => {
                match client.snapshot().await {
                    Ok(snapshot) => {
                        app.update(Msg::Snapshot(snapshot));
                    }
                    Err(err) => {
                        app.update(Msg::Done(Err(format!("lost the daemon: {err}; reconnecting"))));
                        if let Ok(new) = attach().await {
                            client = new;
                        }
                    }
                }
                app.tick()
            }
        };
        if app.quit {
            return Ok(());
        }
        for cmd in cmds {
            run_cmd(cmd, &mut client, &mut app, &tx).await;
        }
    }
}

async fn run_cmd(cmd: Cmd, client: &mut Client, app: &mut App, tx: &mpsc::UnboundedSender<Msg>) {
    match cmd {
        Cmd::Request { request, success } => {
            let result = client
                .request(request)
                .await
                .map(|_| success)
                .map_err(|e| e.to_string());
            app.update(Msg::Done(result));
            // Show the effect right away instead of at the next tick.
            if let Ok(snapshot) = client.snapshot().await {
                app.update(Msg::Snapshot(snapshot));
            }
        }
        Cmd::ListContexts => {
            let result = match client.request(Request::ListContexts).await {
                Ok(Response::Contexts { names, current }) => Ok((names, current)),
                Ok(_) => Err("unexpected answer from the daemon".into()),
                Err(err) => Err(err.to_string()),
            };
            app.update(Msg::Contexts(result));
        }
        Cmd::Discover {
            context,
            namespace_filter,
        } => {
            // Listing a cluster takes seconds; the list stays usable meanwhile.
            let tx = tx.clone();
            tokio::spawn(async move {
                let result = async {
                    let socket = paths::usable_socket_path()?;
                    let mut client = Client::connect(&socket).await.map_err(|e| e.to_string())?;
                    match client
                        .request(Request::Discover {
                            context,
                            namespace_filter,
                        })
                        .await
                        .map_err(|e| e.to_string())?
                    {
                        Response::Discovered { ports } => Ok(ports),
                        _ => Err("unexpected answer from the daemon".to_owned()),
                    }
                }
                .await;
                let _ = tx.send(Msg::Discovered(result));
            });
        }
        Cmd::Events { id } => {
            let result = match client.request(Request::Events { ids: vec![id] }).await {
                Ok(Response::Events { entries }) => Ok(entries),
                Ok(_) => Err("unexpected answer from the daemon".into()),
                Err(err) => Err(err.to_string()),
            };
            app.update(Msg::Events(result));
        }
        Cmd::Open(url) => {
            let opener = if cfg!(target_os = "macos") {
                "open"
            } else {
                "xdg-open"
            };
            let result = Command::new(opener)
                .arg(&url)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .map(|_| format!("opened {url}"))
                .map_err(|err| format!("cannot run {opener}: {err}"));
            app.update(Msg::Done(result));
        }
        Cmd::Copy(text) => {
            let result = copy(&text).map(|()| format!("copied {text}"));
            app.update(Msg::Done(result));
        }
    }
}

/// Puts `text` on the clipboard with the platform's tool.
fn copy(text: &str) -> Result<(), String> {
    use std::io::Write;
    let tools: &[(&str, &[&str])] = if cfg!(target_os = "macos") {
        &[("pbcopy", &[])]
    } else {
        &[("wl-copy", &[]), ("xclip", &["-selection", "clipboard"])]
    };
    for (tool, args) in tools {
        let Ok(mut child) = Command::new(tool)
            .args(*args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            continue;
        };
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(text.as_bytes());
        }
        return match child.wait() {
            Ok(status) if status.success() => Ok(()),
            _ => Err(format!("{tool} failed")),
        };
    }
    Err("no clipboard tool found (pbcopy, wl-copy or xclip)".into())
}
