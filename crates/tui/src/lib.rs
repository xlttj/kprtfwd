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
use kprtfwd_proto::{Client, ClientError, Request, Response, Snapshot, connect_or_start};
use ratatui::Terminal;
use ratatui::backend::Backend;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::widgets::Clear;
use tokio::sync::mpsc;

use crate::app::{App, Cmd, Msg};

const REFRESH: Duration = Duration::from_millis(300);

pub async fn run() -> Result<()> {
    // A broken config.toml is reported now, not only when quitting.
    let config_path = paths::config_path().context("neither KPRTFWD_HOME nor HOME is set")?;
    let config = Config::load(&config_path)?;
    let mut daemon = Connection(attach().await?);
    let snapshot = daemon.snapshot().await?;
    let mut app = App::new(snapshot);
    app.keep_forwards_running = config.keep_forwards_running;
    app.config_path = home_relative(&config_path);

    let mut terminal = ratatui::init();
    let (tx, rx) = mpsc::unbounded_channel::<Msg>();
    read_terminal(tx.clone());
    let result = event_loop(&mut terminal, &mut daemon, &mut app, rx, tx).await;
    ratatui::restore();
    result
}

/// What the TUI needs from the daemon; a fake in tests.
trait Daemon {
    async fn snapshot(&mut self) -> Result<Snapshot, ClientError>;

    async fn request(&mut self, request: Request) -> Result<Response, ClientError>;

    /// Connects again, after the connection was lost.
    async fn reconnect(&mut self) -> Result<()>;

    /// Sends a request on a connection of its own, for slow ones: the list
    /// stays usable meanwhile.
    fn request_aside(
        &self,
        request: Request,
    ) -> impl Future<Output = Result<Response, String>> + Send + 'static;
}

/// The daemon over its socket.
struct Connection(Client);

impl Daemon for Connection {
    async fn snapshot(&mut self) -> Result<Snapshot, ClientError> {
        self.0.snapshot().await
    }

    async fn request(&mut self, request: Request) -> Result<Response, ClientError> {
        self.0.request(request).await
    }

    async fn reconnect(&mut self) -> Result<()> {
        self.0 = attach().await?;
        Ok(())
    }

    fn request_aside(
        &self,
        request: Request,
    ) -> impl Future<Output = Result<Response, String>> + Send + 'static {
        let socket = paths::usable_socket_path();
        async move {
            let mut client = Client::connect(&socket?).await.map_err(|e| e.to_string())?;
            client.request(request).await.map_err(|e| e.to_string())
        }
    }
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

/// Terminal input blocks, so it gets its own thread.
fn read_terminal(tx: mpsc::UnboundedSender<Msg>) {
    std::thread::spawn(move || {
        while let Ok(event) = event::read() {
            let msg = match event {
                Event::Key(key) if key.kind == KeyEventKind::Press => Msg::Key(key),
                // Anything else, like a resize, just redraws.
                _ => Msg::Done(Ok(String::new())),
            };
            if tx.send(msg).is_err() {
                return;
            }
        }
    });
}

/// Draws, takes the messages from `rx` and refreshes the snapshot until the
/// user quits. `tx` is for answers that arrive later.
async fn event_loop<B, D>(
    terminal: &mut Terminal<B>,
    daemon: &mut D,
    app: &mut App,
    mut rx: mpsc::UnboundedReceiver<Msg>,
    tx: mpsc::UnboundedSender<Msg>,
) -> Result<()>
where
    B: Backend,
    B::Error: Send + Sync + 'static,
    D: Daemon,
{
    let mut refresh = tokio::time::interval(REFRESH);

    loop {
        terminal.draw(|frame| view::draw(frame, app))?;
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
                match daemon.snapshot().await {
                    Ok(snapshot) => {
                        app.update(Msg::Snapshot(snapshot));
                    }
                    Err(err) => {
                        app.update(Msg::Done(Err(format!("lost the daemon: {err}; reconnecting"))));
                        let _ = daemon.reconnect().await;
                    }
                }
                app.tick()
            }
        };
        if app.quit {
            return Ok(());
        }
        for cmd in cmds {
            run_cmd(cmd, daemon, app, &tx).await;
        }
    }
}

async fn run_cmd(
    cmd: Cmd,
    daemon: &mut impl Daemon,
    app: &mut App,
    tx: &mpsc::UnboundedSender<Msg>,
) {
    match cmd {
        Cmd::Request { request, success } => {
            let result = daemon
                .request(request)
                .await
                .map(|_| success)
                .map_err(|e| e.to_string());
            app.update(Msg::Done(result));
            // Show the effect right away instead of at the next tick.
            if let Ok(snapshot) = daemon.snapshot().await {
                app.update(Msg::Snapshot(snapshot));
            }
        }
        Cmd::ListContexts => {
            let result = match daemon.request(Request::ListContexts).await {
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
            let answer = daemon.request_aside(Request::Discover {
                context,
                namespace_filter,
            });
            let tx = tx.clone();
            tokio::spawn(async move {
                let result = match answer.await {
                    Ok(Response::Discovered { ports }) => Ok(ports),
                    Ok(_) => Err("unexpected answer from the daemon".to_owned()),
                    Err(err) => Err(err),
                };
                let _ = tx.send(Msg::Discovered(result));
            });
        }
        Cmd::Events { id } => {
            let result = match daemon.request(Request::Events { ids: vec![id] }).await {
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

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex, MutexGuard};
    use std::time::SystemTime;

    use kprtfwd_core::ForwardId;
    use kprtfwd_core::svctype::ServiceType;
    use kprtfwd_proto::{DiscoveredPort, EventEntry};
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{KeyEvent, KeyEventState};
    use tokio::sync::Semaphore;

    use super::*;
    use crate::app::tests::{key, snapshot};
    use crate::app::{Discovery, Mode};

    /// A daemon that answers from memory and notes every call.
    #[derive(Clone)]
    struct FakeDaemon {
        state: Arc<Mutex<State>>,
        /// Each discovery waits for a permit, so a test decides when it ends.
        discovery: Arc<Semaphore>,
    }

    struct State {
        calls: Vec<String>,
        snapshot: Snapshot,
        /// While false, every call fails like a closed connection.
        connected: bool,
        /// Whether reconnecting works.
        daemon_back: bool,
        /// The daemon's error for every request.
        refuse: Option<String>,
        events: Vec<EventEntry>,
    }

    impl FakeDaemon {
        fn new() -> Self {
            Self {
                state: Arc::new(Mutex::new(State {
                    calls: Vec::new(),
                    snapshot: snapshot(),
                    connected: true,
                    daemon_back: true,
                    refuse: None,
                    events: Vec::new(),
                })),
                discovery: Arc::new(Semaphore::new(0)),
            }
        }

        fn state(&self) -> MutexGuard<'_, State> {
            self.state.lock().unwrap()
        }

        fn calls(&self) -> Vec<String> {
            self.state().calls.clone()
        }

        fn count(&self, call: &str) -> usize {
            self.calls().iter().filter(|c| *c == call).count()
        }
    }

    /// `SetEnabled { .. }` as `SetEnabled`.
    fn name(request: &Request) -> String {
        let text = format!("{request:?}");
        text.split([' ', '{', '(']).next().unwrap().to_owned()
    }

    impl Daemon for FakeDaemon {
        async fn snapshot(&mut self) -> Result<Snapshot, ClientError> {
            let mut state = self.state();
            state.calls.push("snapshot".into());
            if !state.connected {
                return Err(ClientError::Closed);
            }
            Ok(state.snapshot.clone())
        }

        async fn request(&mut self, request: Request) -> Result<Response, ClientError> {
            let mut state = self.state();
            state.calls.push(name(&request));
            if !state.connected {
                return Err(ClientError::Closed);
            }
            if let Some(message) = &state.refuse {
                return Err(ClientError::Daemon(message.clone()));
            }
            Ok(match request {
                Request::SetEnabled { ids, enabled } => {
                    for view in &mut state.snapshot.forwards {
                        if ids.contains(&view.forward.id) {
                            view.forward.enabled = enabled;
                        }
                    }
                    Response::Done
                }
                Request::ListContexts => Response::Contexts {
                    names: vec!["staging".into()],
                    current: None,
                },
                Request::Events { .. } => Response::Events {
                    entries: state.events.clone(),
                },
                _ => Response::Done,
            })
        }

        async fn reconnect(&mut self) -> Result<()> {
            let mut state = self.state();
            state.calls.push("reconnect".into());
            state.connected = state.daemon_back;
            if state.connected {
                Ok(())
            } else {
                Err(ClientError::NotRunning(std::io::ErrorKind::NotFound.into()).into())
            }
        }

        fn request_aside(
            &self,
            request: Request,
        ) -> impl Future<Output = Result<Response, String>> + Send + 'static {
            let fake = self.clone();
            async move {
                fake.state().calls.push(format!("aside {}", name(&request)));
                fake.discovery.acquire().await.unwrap().forget();
                let Request::Discover {
                    namespace_filter, ..
                } = request
                else {
                    return Err("unexpected request".into());
                };
                Ok(Response::Discovered {
                    ports: vec![DiscoveredPort {
                        namespace: namespace_filter.clone(),
                        service: "api".into(),
                        port: 80,
                        port_name: None,
                        service_type: ServiceType::Http,
                        existing: None,
                        id: ForwardId::new(format!("staging.{namespace_filter}.api-80")),
                        local_port: 10080,
                    }],
                })
            }
        }
    }

    fn ctrl(c: char) -> Msg {
        Msg::Key(KeyEvent {
            code: KeyCode::Char(c),
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        })
    }

    fn char_key(c: char) -> Msg {
        key(KeyCode::Char(c))
    }

    /// Sends messages to the loop as the terminal thread does.
    struct Keys(mpsc::UnboundedSender<Msg>);

    impl Keys {
        /// Sends `msg` and lets the loop take it.
        async fn send(&self, msg: Msg) {
            self.0.send(msg).unwrap();
            tokio::time::sleep(Duration::from_millis(1)).await;
        }

        async fn type_text(&self, text: &str) {
            for c in text.chars() {
                self.send(char_key(c)).await;
            }
        }

        /// Lets `n` refresh ticks pass.
        async fn ticks(&self, n: u32) {
            tokio::time::sleep(REFRESH * n).await;
        }
    }

    /// Runs the loop with `fake` until `script` is done, then quits with
    /// ctrl+c; returns the terminal with the last frame.
    async fn run_loop(
        fake: &FakeDaemon,
        app: &mut App,
        script: impl AsyncFnOnce(&Keys),
    ) -> Terminal<TestBackend> {
        let mut terminal = Terminal::new(TestBackend::new(120, 20)).unwrap();
        let (tx, rx) = mpsc::unbounded_channel();
        let keys = Keys(tx.clone());
        let mut daemon = fake.clone();
        app.quit = false;
        let script = async {
            script(&keys).await;
            keys.send(ctrl('c')).await;
        };
        let (result, ()) =
            tokio::join!(event_loop(&mut terminal, &mut daemon, app, rx, tx), script);
        result.unwrap();
        terminal
    }

    fn screen(terminal: &Terminal<TestBackend>) -> String {
        let buffer = terminal.backend().buffer();
        buffer
            .content()
            .chunks(buffer.area.width as usize)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[tokio::test(start_paused = true)]
    async fn a_request_is_reported_and_its_effect_shown_at_once() {
        let fake = FakeDaemon::new();
        let mut app = App::new(snapshot());
        run_loop(&fake, &mut app, async |keys| {
            keys.send(char_key(' ')).await;
        })
        .await;

        let calls = fake.calls();
        let at = calls.iter().position(|c| c == "SetEnabled").unwrap();
        assert_eq!(calls[at + 1], "snapshot", "{calls:?}");
        let message = app.message.as_ref().unwrap();
        assert_eq!(
            (message.text.as_str(), message.error),
            ("on: prd-web", false)
        );
        assert!(app.snapshot.forwards[0].forward.enabled);
    }

    #[tokio::test(start_paused = true)]
    async fn a_refused_request_shows_the_daemons_reason() {
        let fake = FakeDaemon::new();
        fake.state().refuse = Some("port 10080 is taken".into());
        let mut app = App::new(snapshot());
        run_loop(&fake, &mut app, async |keys| {
            keys.send(char_key(' ')).await;
        })
        .await;
        let message = app.message.as_ref().unwrap();
        assert_eq!(
            (message.text.as_str(), message.error),
            ("port 10080 is taken", true)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_lost_daemon_is_reported_and_reconnected() {
        let fake = FakeDaemon::new();
        {
            let mut state = fake.state();
            state.connected = false;
            state.daemon_back = false;
        }
        let mut app = App::new(snapshot());
        run_loop(&fake, &mut app, async |keys| keys.ticks(3).await).await;
        let message = app.message.as_ref().unwrap();
        assert_eq!(
            message.text,
            "lost the daemon: the daemon closed the connection; reconnecting"
        );
        assert!(message.error);
        // Every tick tries again.
        assert!(fake.count("reconnect") >= 3, "{:?}", fake.calls());

        // Back, the daemon's state shows at the next tick.
        {
            let mut state = fake.state();
            state.daemon_back = true;
            state.snapshot.paused = true;
        }
        run_loop(&fake, &mut app, async |keys| keys.ticks(2).await).await;
        assert!(app.snapshot.paused);
        let calls = fake.calls();
        let back = calls.iter().rposition(|c| c == "reconnect").unwrap();
        assert_eq!(calls[back + 1], "snapshot", "{calls:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_discovery_answer_after_esc_is_dropped() {
        let fake = FakeDaemon::new();
        let mut app = App::new(snapshot());
        let discover = async |keys: &Keys, namespace: &str| {
            keys.send(char_key('d')).await;
            keys.send(key(KeyCode::Enter)).await;
            keys.type_text(namespace).await;
            keys.send(key(KeyCode::Enter)).await;
        };
        run_loop(&fake, &mut app, async |keys| {
            discover(keys, "shop").await;
            // The list is usable while discovery runs.
            keys.send(key(KeyCode::Esc)).await;
            keys.send(char_key('j')).await;
            fake.discovery.add_permits(1);
            keys.ticks(1).await;
        })
        .await;
        assert!(matches!(app.mode, Mode::List));
        assert_eq!(app.selected, Some(ForwardId::new("stg-db")));
        assert_eq!(fake.count("aside Discover"), 1);

        run_loop(&fake, &mut app, async |keys| {
            discover(keys, "web").await;
            fake.discovery.add_permits(1);
            keys.ticks(1).await;
        })
        .await;
        let Mode::Discovery(Discovery::Services(services)) = &app.mode else {
            panic!("no services shown");
        };
        assert_eq!(services.context, "staging");
        assert_eq!(services.ports[0].port.namespace, "web");
    }

    #[tokio::test(start_paused = true)]
    async fn ctrl_l_and_resizes_only_redraw() {
        let fake = FakeDaemon::new();
        let mut app = App::new(snapshot());
        let terminal = run_loop(&fake, &mut app, async |keys| {
            keys.send(char_key(' ')).await;
            // A resize arrives as an empty Done.
            keys.send(Msg::Done(Ok(String::new()))).await;
            keys.send(ctrl('l')).await;
        })
        .await;
        assert!(matches!(app.mode, Mode::List), "ctrl+l opened a view");
        assert_eq!(app.message.as_ref().unwrap().text, "on: prd-web");
        assert_eq!(fake.count("Events"), 0);
        let screen = screen(&terminal);
        assert!(screen.contains("prd-web"), "{screen}");
        assert!(screen.contains("on: prd-web"), "{screen}");
    }

    #[tokio::test(start_paused = true)]
    async fn the_events_view_refreshes_on_every_tick() {
        let fake = FakeDaemon::new();
        fake.state().events = vec![EventEntry {
            at: SystemTime::UNIX_EPOCH,
            source: "prd-web".into(),
            message: "pod web-0".into(),
        }];
        let mut app = App::new(snapshot());
        run_loop(&fake, &mut app, async |keys| {
            keys.send(char_key('l')).await;
            keys.ticks(3).await;
            assert!(fake.count("Events") >= 3, "{:?}", fake.calls());
            keys.send(key(KeyCode::Esc)).await;
            let seen = fake.count("Events");
            keys.ticks(3).await;
            assert_eq!(fake.count("Events"), seen, "refreshed after leaving");
        })
        .await;
        assert!(matches!(app.mode, Mode::List));

        // An error ends the view.
        run_loop(&fake, &mut app, async |keys| {
            keys.send(char_key('l')).await;
            fake.state().refuse = Some("no events".into());
            keys.ticks(2).await;
        })
        .await;
        assert!(matches!(app.mode, Mode::List));
        let message = app.message.as_ref().unwrap();
        assert_eq!((message.text.as_str(), message.error), ("no events", true));
    }
}
