//! The kprtfwd terminal UI: one list of all forwards across all contexts.
//!
//! It asks the daemon for a snapshot every 300 ms; the local socket makes that
//! cheap, and it keeps the UI free of a push protocol. Daemon calls run one at
//! a time next to the keys, each with a time limit, so a slow or lost daemon
//! never holds up the keys.

mod app;
mod view;

use std::collections::VecDeque;
use std::pin::Pin;
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use kprtfwd_core::{Config, paths};
use kprtfwd_proto::{Client, ClientError, Request, Response, Snapshot, connect_or_start};
use ratatui::Terminal;
use ratatui::backend::Backend;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::widgets::Clear;
use tokio::sync::mpsc;
use tokio::time::{Instant, MissedTickBehavior, timeout};

use crate::app::{App, Cmd, Found, Msg};

const REFRESH: Duration = Duration::from_millis(300);
/// How long the daemon may take to answer a snapshot or a request. It needs
/// milliseconds; a daemon that takes longer counts as lost.
const ANSWER_WITHIN: Duration = Duration::from_secs(5);
/// How long connecting may take, a daemon start included (which
/// `connect_or_start` waits 10 s for).
const CONNECT_WITHIN: Duration = Duration::from_secs(15);
/// The pause between two attempts to reach a lost daemon.
const RECONNECT_EVERY: Duration = Duration::from_secs(1);
/// How long discovery may take: the daemon gives a browser login 5 minutes.
const DISCOVER_WITHIN: Duration = Duration::from_secs(6 * 60);
/// How long a clipboard tool that failed may take to close its stderr.
const REASON_WITHIN: Duration = Duration::from_millis(200);
/// The clipboard tools, tried in turn until one works.
const CLIPBOARD: &[(&str, &[&str])] = if cfg!(target_os = "macos") {
    &[("pbcopy", &[])]
} else {
    &[
        ("wl-copy", &[]),
        ("xclip", &["-selection", "clipboard"]),
        ("xsel", &["--clipboard", "--input"]),
    ]
};
const NOT_CONNECTED: &str = "not connected to the daemon";
const UNEXPECTED: &str = "unexpected answer from the daemon";

pub async fn run() -> Result<()> {
    // A broken config.toml is reported now, not only when quitting.
    let config_path = paths::config_path().context("neither KPRTFWD_HOME nor HOME is set")?;
    let config = Config::load(&config_path)?;
    let mut client = timeout(CONNECT_WITHIN, attach())
        .await
        .map_err(|_| anyhow!(no_answer(CONNECT_WITHIN)))??;
    let snapshot = timeout(ANSWER_WITHIN, client.snapshot())
        .await
        .map_err(|_| anyhow!(no_answer(ANSWER_WITHIN)))??;
    let mut app = App::new(snapshot);
    app.keep_forwards_running = config.keep_forwards_running;
    app.config_path = home_relative(&config_path);

    let (tx, rx) = mpsc::unbounded_channel::<Msg>();
    quit_on_signals(tx.clone())?;
    let mut terminal = ratatui::init();
    read_terminal(tx.clone());
    let result = event_loop(&mut terminal, Socket, client, &mut app, rx, tx).await;
    ratatui::restore();
    result
}

/// The daemon, as the TUI needs it; a fake in tests.
trait Daemon {
    type Conn: Connection;

    /// Connects as a TUI, starting the daemon if needed.
    fn attach(&self) -> impl Future<Output = Result<Self::Conn, String>> + 'static;

    /// Sends a request on a connection of its own, for slow ones: the list
    /// stays usable meanwhile.
    fn request_aside(
        &self,
        request: Request,
    ) -> impl Future<Output = Result<Response, String>> + Send + 'static;
}

/// One connection to the daemon.
trait Connection: 'static {
    async fn snapshot(&mut self) -> Result<Snapshot, ClientError>;

    async fn request(&mut self, request: Request) -> Result<Response, ClientError>;
}

/// The daemon on its socket.
struct Socket;

impl Daemon for Socket {
    type Conn = Client;

    fn attach(&self) -> impl Future<Output = Result<Client, String>> + 'static {
        let attaching = attach();
        async move { attaching.await.map_err(|err| format!("{err:#}")) }
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

impl Connection for Client {
    async fn snapshot(&mut self) -> Result<Snapshot, ClientError> {
        Client::snapshot(self).await
    }

    async fn request(&mut self, request: Request) -> Result<Response, ClientError> {
        Client::request(self, request).await
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

/// SIGTERM and SIGINT end the TUI like q, so the terminal is restored. (In
/// raw mode ctrl+c is a key, not SIGINT.) A closed terminal still ends it
/// with SIGHUP; there is nothing left to restore.
fn quit_on_signals(tx: mpsc::UnboundedSender<Msg>) -> Result<()> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut terminate = signal(SignalKind::terminate())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    tokio::spawn(async move {
        tokio::select! {
            _ = terminate.recv() => {}
            _ = interrupt.recv() => {}
        }
        let _ = tx.send(Msg::Quit);
    });
    Ok(())
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
    daemon: D,
    conn: D::Conn,
    app: &mut App,
    mut rx: mpsc::UnboundedReceiver<Msg>,
    tx: mpsc::UnboundedSender<Msg>,
) -> Result<()>
where
    B: Backend,
    B::Error: Send + Sync + 'static,
    D: Daemon,
{
    let mut link = Link::new(daemon, conn);
    let mut refresh = tokio::time::interval(REFRESH);
    // A tick missed while the loop was busy is not made up for.
    refresh.set_missed_tick_behavior(MissedTickBehavior::Skip);

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
            finished = link.finished() => link.finish(finished, app),
            _ = refresh.tick() => {
                link.refresh();
                app.tick()
            }
        };
        if app.quit {
            return Ok(());
        }
        for cmd in cmds {
            run_cmd(cmd, &mut link, app, &tx);
        }
        link.start_next();
    }
}

/// A daemon call in flight. It owns the connection and gives it back.
type Call<C> = Pin<Box<dyn Future<Output = Finished<C>>>>;

/// What a daemon call gives back.
struct Finished<C> {
    conn: C,
    msgs: Vec<Msg>,
    /// Why the connection is lost, if it is.
    lost: Option<String>,
}

/// The event loop's connection to the daemon. One call runs at a time, with
/// a time limit, while the loop goes on taking keys.
struct Link<D: Daemon> {
    daemon: D,
    /// The connection while no call has it. While the daemon is lost it is
    /// the old one, kept open until a new one has attached: the daemon must
    /// not see this TUI leave in between and switch the forwards off.
    idle: Option<D::Conn>,
    call: Option<Call<D::Conn>>,
    /// Daemon commands waiting for the call in flight.
    queue: VecDeque<Cmd>,
    /// While the daemon is lost: when to try to reach it again.
    retry_at: Option<Instant>,
}

impl<D: Daemon> Link<D> {
    fn new(daemon: D, conn: D::Conn) -> Self {
        Self {
            daemon,
            idle: Some(conn),
            call: None,
            queue: VecDeque::new(),
            retry_at: None,
        }
    }

    /// The result of the call in flight; never ready without one.
    async fn finished(&mut self) -> Finished<D::Conn> {
        match &mut self.call {
            Some(call) => call.await,
            None => std::future::pending().await,
        }
    }

    /// A fresh snapshot, unless the connection is busy or lost.
    fn refresh(&mut self) {
        if self.retry_at.is_none() && self.queue.is_empty() && self.call.is_none() {
            self.start(Work::Refresh);
        }
    }

    /// Takes a daemon command; it runs once the connection is free. While
    /// the daemon is lost it fails at once.
    fn push(&mut self, cmd: Cmd, app: &mut App) {
        if self.retry_at.is_some() {
            app.update(refused(cmd, NOT_CONNECTED));
            return;
        }
        // The events view asks on every tick; once in the queue is enough.
        if matches!(cmd, Cmd::Events { .. }) && self.queue.contains(&cmd) {
            return;
        }
        self.queue.push_back(cmd);
    }

    /// Starts what is due once the connection is free: the next command, or
    /// while the daemon is lost, the next attempt to reach it.
    fn start_next(&mut self) {
        if self.call.is_some() {
            return;
        }
        if let Some(cmd) = self.queue.pop_front() {
            self.start(Work::Cmd(cmd));
        } else if self.retry_at.is_some_and(|at| at <= Instant::now()) {
            self.start(Work::Reconnect);
        }
    }

    fn start(&mut self, work: Work) {
        let Some(conn) = self.idle.take() else {
            return;
        };
        self.call = Some(match work {
            Work::Refresh => Box::pin(refreshed(conn)),
            Work::Cmd(cmd) => Box::pin(ask(conn, cmd)),
            Work::Reconnect => Box::pin(reconnect(conn, self.daemon.attach())),
        });
    }

    /// Takes the result of the call that finished.
    fn finish(&mut self, finished: Finished<D::Conn>, app: &mut App) -> Vec<Cmd> {
        self.call = None;
        self.idle = Some(finished.conn);
        let cmds = finished
            .msgs
            .into_iter()
            .flat_map(|msg| app.update(msg))
            .collect();
        match finished.lost {
            None => self.retry_at = None,
            Some(reason) => {
                for cmd in self.queue.drain(..) {
                    app.update(refused(cmd, NOT_CONNECTED));
                }
                app.update(Msg::Lost(reason));
                // Just lost: try again at once, a restart takes a moment.
                let wait = if self.retry_at.is_some() {
                    RECONNECT_EVERY
                } else {
                    Duration::ZERO
                };
                self.retry_at = Some(Instant::now() + wait);
            }
        }
        cmds
    }
}

/// What a daemon call does.
enum Work {
    Refresh,
    Cmd(Cmd),
    Reconnect,
}

/// The answer to a daemon command that cannot be sent.
fn refused(cmd: Cmd, reason: &str) -> Msg {
    match cmd {
        Cmd::ListContexts => Msg::Contexts(Err(reason.into())),
        Cmd::Events { .. } => Msg::Events(Err(reason.into())),
        Cmd::Edit { id, .. } => Msg::Saved {
            id,
            result: Err(reason.into()),
        },
        // The others do not go over the connection; only requests are left.
        _ => Msg::Done(Err(reason.into())),
    }
}

/// A daemon call with its time limit. The outer error says why the
/// connection is lost; the daemon's own refusal is the inner one.
async fn bounded<T>(
    call: impl Future<Output = Result<T, ClientError>>,
) -> Result<Result<T, String>, String> {
    match timeout(ANSWER_WITHIN, call).await {
        Ok(Ok(value)) => Ok(Ok(value)),
        Ok(Err(ClientError::Daemon(message))) => Ok(Err(message)),
        Ok(Err(err)) => Err(err.to_string()),
        Err(_) => Err(no_answer(ANSWER_WITHIN)),
    }
}

/// The answer of a [`bounded`] call, and why the connection is lost, if it is.
fn split<T>(result: Result<Result<T, String>, String>) -> (Result<T, String>, Option<String>) {
    match result {
        Ok(answer) => (answer, None),
        Err(reason) => (Err(reason.clone()), Some(reason)),
    }
}

fn no_answer(limit: Duration) -> String {
    let secs = limit.as_secs();
    if secs >= 60 && secs.is_multiple_of(60) {
        format!("the daemon did not answer within {} min", secs / 60)
    } else {
        format!("the daemon did not answer within {secs} s")
    }
}

/// Asks for a snapshot; returns why the connection is lost, if it is.
async fn fetch_snapshot(conn: &mut impl Connection, msgs: &mut Vec<Msg>) -> Option<String> {
    let (answer, lost) = split(bounded(conn.snapshot()).await);
    match answer {
        Ok(snapshot) => msgs.push(Msg::Snapshot(snapshot)),
        Err(message) if lost.is_none() => msgs.push(Msg::Done(Err(message))),
        Err(_) => {}
    }
    lost
}

async fn refreshed<C: Connection>(mut conn: C) -> Finished<C> {
    let mut msgs = Vec::new();
    let lost = fetch_snapshot(&mut conn, &mut msgs).await;
    Finished { conn, msgs, lost }
}

/// Sends a daemon command.
async fn ask<C: Connection>(mut conn: C, cmd: Cmd) -> Finished<C> {
    let mut msgs = Vec::new();
    let lost = match cmd {
        Cmd::Request { request, success } => {
            let (answer, lost) = split(bounded(conn.request(request)).await);
            msgs.push(Msg::Done(answer.map(|_| success)));
            match lost {
                // Show the effect right away instead of at the next tick.
                None => fetch_snapshot(&mut conn, &mut msgs).await,
                lost => lost,
            }
        }
        Cmd::Edit { id, settings } => {
            let request = Request::EditForward {
                id: id.clone(),
                settings,
            };
            let (answer, lost) = split(bounded(conn.request(request)).await);
            let result = answer.and_then(|response| match response {
                Response::Saved { id } => Ok(id),
                _ => Err(UNEXPECTED.into()),
            });
            msgs.push(Msg::Saved { id, result });
            match lost {
                None => fetch_snapshot(&mut conn, &mut msgs).await,
                lost => lost,
            }
        }
        Cmd::ListContexts => {
            let (answer, lost) = split(bounded(conn.request(Request::ListContexts)).await);
            msgs.push(Msg::Contexts(answer.and_then(|response| match response {
                Response::Contexts { names, current } => Ok((names, current)),
                _ => Err(UNEXPECTED.into()),
            })));
            lost
        }
        Cmd::Events { id } => {
            let request = Request::Events { ids: vec![id] };
            let (answer, lost) = split(bounded(conn.request(request)).await);
            msgs.push(Msg::Events(answer.and_then(|response| match response {
                Response::Events { entries } => Ok(entries),
                _ => Err(UNEXPECTED.into()),
            })));
            lost
        }
        cmd => {
            msgs.push(refused(cmd, "not a daemon command"));
            None
        }
    };
    Finished { conn, msgs, lost }
}

/// Connects again. `old` stays open until the new connection has attached.
async fn reconnect<C: Connection>(
    old: C,
    attach: impl Future<Output = Result<C, String>>,
) -> Finished<C> {
    let lost = match timeout(CONNECT_WITHIN, attach).await {
        Ok(Ok(mut conn)) => {
            drop(old);
            let mut msgs = Vec::new();
            let lost = fetch_snapshot(&mut conn, &mut msgs).await;
            return Finished { conn, msgs, lost };
        }
        Ok(Err(reason)) => reason,
        Err(_) => no_answer(CONNECT_WITHIN),
    };
    Finished {
        conn: old,
        msgs: Vec::new(),
        lost: Some(lost),
    }
}

/// Runs a command: daemon commands go to `link`, the others run here.
fn run_cmd<D: Daemon>(
    cmd: Cmd,
    link: &mut Link<D>,
    app: &mut App,
    tx: &mpsc::UnboundedSender<Msg>,
) {
    match cmd {
        Cmd::Request { .. } | Cmd::Edit { .. } | Cmd::ListContexts | Cmd::Events { .. } => {
            link.push(cmd, app);
        }
        Cmd::Discover {
            context,
            namespace_filter,
            request,
        } => {
            // Listing a cluster takes seconds; the list stays usable meanwhile.
            let answer = timeout(
                DISCOVER_WITHIN,
                link.daemon.request_aside(Request::Discover {
                    context,
                    namespace_filter,
                }),
            );
            let tx = tx.clone();
            tokio::spawn(async move {
                let result = match answer.await {
                    Ok(Ok(Response::Discovered { ports, skipped })) => Ok(Found { ports, skipped }),
                    Ok(Ok(_)) => Err(UNEXPECTED.to_owned()),
                    Ok(Err(err)) => Err(err),
                    Err(_) => Err(no_answer(DISCOVER_WITHIN)),
                };
                let _ = tx.send(Msg::Discovered { request, result });
            });
        }
        Cmd::Open(url) => {
            let opener = if cfg!(target_os = "macos") {
                "open"
            } else {
                "xdg-open"
            };
            app.update(Msg::Done(open(opener, &url, tx.clone())));
        }
        Cmd::Copy(text) => {
            let result = copy(CLIPBOARD, &text).map(|()| format!("copied {text}"));
            app.update(Msg::Done(result));
        }
    }
}

/// Opens `url` with `opener`. A thread waits for the opener, so it does not
/// stay behind as a zombie, and reports through `tx` when it fails.
fn open(opener: &str, url: &str, tx: mpsc::UnboundedSender<Msg>) -> Result<String, String> {
    let mut child = Command::new(opener)
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|err| format!("cannot run {opener}: {err}"))?;
    let failed = format!("{opener} could not open {url}");
    std::thread::spawn(move || {
        if let Ok(status) = child.wait()
            && !status.success()
        {
            let _ = tx.send(Msg::Done(Err(format!("{failed} ({status})"))));
        }
    });
    Ok(format!("opened {url}"))
}

/// Puts `text` on the clipboard with the first of `tools` that works: one
/// that is missing or fails hands over to the next. When all fail, says why
/// each one did.
fn copy(tools: &[(&str, &[&str])], text: &str) -> Result<(), String> {
    let mut failures = Vec::new();
    for (tool, args) in tools {
        match copy_with(tool, args, text) {
            Ok(()) => return Ok(()),
            Err(Some(why)) => failures.push(why),
            Err(None) => {}
        }
    }
    if failures.is_empty() {
        let names: Vec<&str> = tools.iter().map(|(tool, _)| *tool).collect();
        return Err(format!("no clipboard tool found ({})", names.join(", ")));
    }
    Err(failures.join("; "))
}

/// Puts `text` on the clipboard with `tool`. `Err(None)` when the tool is
/// not installed, otherwise why it failed.
fn copy_with(tool: &str, args: &[&str], text: &str) -> Result<(), Option<String>> {
    use std::io::{Read, Write};
    let mut child = match Command::new(tool)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Err(None),
        Err(err) => return Err(Some(format!("cannot run {tool}: {err}"))),
    };
    // wl-copy, xclip and xsel leave a process behind that serves the
    // clipboard and keeps stderr open. So stderr is read aside, and only
    // waited for when the tool failed, which happens before that process
    // starts.
    let (said, heard) = std::sync::mpsc::channel();
    if let Some(mut stderr) = child.stderr.take() {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = stderr.read_to_end(&mut bytes);
            let _ = said.send(String::from_utf8_lossy(&bytes).into_owned());
        });
    }
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(text.as_bytes());
    }
    let status = child.wait().map_err(|err| Some(format!("{tool}: {err}")))?;
    if status.success() {
        return Ok(());
    }
    let reason = heard.recv_timeout(REASON_WITHIN).unwrap_or_default();
    Err(Some(
        match reason.lines().map(str::trim).find(|line| !line.is_empty()) {
            Some(line) => format!("{tool}: {line}"),
            None => format!("{tool} failed ({status})"),
        },
    ))
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
        /// Connections opened and closed, in order: `open 1`, `close 1`.
        links: Vec<String>,
        opened: usize,
        snapshot: Snapshot,
        /// While false, every call fails like a closed connection.
        connected: bool,
        /// Whether reconnecting works, and why not.
        daemon_back: bool,
        attach_error: String,
        /// The daemon's error for every request.
        refuse: Option<String>,
        /// Calls that never get an answer, by the names in `calls`.
        stall: Vec<&'static str>,
        events: Vec<EventEntry>,
        /// The ID an edited forward gets; `None` keeps its own.
        saved_as: Option<ForwardId>,
    }

    impl FakeDaemon {
        fn new() -> Self {
            Self {
                state: Arc::new(Mutex::new(State {
                    calls: Vec::new(),
                    links: Vec::new(),
                    opened: 0,
                    snapshot: snapshot(),
                    connected: true,
                    daemon_back: true,
                    attach_error: ClientError::NotRunning(std::io::ErrorKind::NotFound.into())
                        .to_string(),
                    refuse: None,
                    stall: Vec::new(),
                    events: Vec::new(),
                    saved_as: None,
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

        fn connect(&self) -> FakeConnection {
            let mut state = self.state();
            state.opened += 1;
            let id = state.opened;
            state.links.push(format!("open {id}"));
            FakeConnection {
                fake: self.clone(),
                id,
            }
        }

        /// Notes `call`; never returns if the daemon does not answer it.
        async fn take(&self, call: String) {
            let stalled = {
                let mut state = self.state();
                let stalled = state.stall.contains(&call.as_str());
                state.calls.push(call);
                stalled
            };
            if stalled {
                std::future::pending::<()>().await;
            }
        }
    }

    /// `SetEnabled { .. }` as `SetEnabled`.
    fn name(request: &Request) -> String {
        let text = format!("{request:?}");
        text.split([' ', '{', '(']).next().unwrap().to_owned()
    }

    impl Daemon for FakeDaemon {
        type Conn = FakeConnection;

        fn attach(&self) -> impl Future<Output = Result<FakeConnection, String>> + 'static {
            let fake = self.clone();
            async move {
                fake.take("reconnect".into()).await;
                let refusal = {
                    let mut state = fake.state();
                    state.connected = state.daemon_back;
                    (!state.connected).then(|| state.attach_error.clone())
                };
                match refusal {
                    None => Ok(fake.connect()),
                    Some(reason) => Err(reason),
                }
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
                    skipped: Vec::new(),
                })
            }
        }
    }

    /// A connection to the fake; notes when it closes.
    struct FakeConnection {
        fake: FakeDaemon,
        id: usize,
    }

    impl Drop for FakeConnection {
        fn drop(&mut self) {
            self.fake.state().links.push(format!("close {}", self.id));
        }
    }

    impl Connection for FakeConnection {
        async fn snapshot(&mut self) -> Result<Snapshot, ClientError> {
            self.fake.take("snapshot".into()).await;
            let state = self.fake.state();
            if !state.connected {
                return Err(ClientError::Closed);
            }
            Ok(state.snapshot.clone())
        }

        async fn request(&mut self, request: Request) -> Result<Response, ClientError> {
            self.fake.take(name(&request)).await;
            let mut state = self.fake.state();
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
                Request::EditForward { id, settings } => {
                    // Saved under the ID the test gives, else its own.
                    let saved = state.saved_as.clone().unwrap_or_else(|| id.clone());
                    for view in &mut state.snapshot.forwards {
                        if view.forward.id == id {
                            let enabled = view.forward.enabled;
                            view.forward = settings.clone().into_forward(saved.clone(), enabled);
                        }
                    }
                    Response::Saved { id: saved }
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
        let conn = fake.connect();
        app.quit = false;
        let script = async {
            script(&keys).await;
            keys.send(ctrl('c')).await;
        };
        let (result, ()) = tokio::join!(
            event_loop(&mut terminal, fake.clone(), conn, app, rx, tx),
            script
        );
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
    async fn an_edit_is_saved_and_its_effect_shown_at_once() {
        let fake = FakeDaemon::new();
        let mut app = App::new(snapshot());
        run_loop(&fake, &mut app, async |keys| {
            keys.send(char_key('e')).await;
            keys.send(key(KeyCode::Backspace)).await;
            keys.type_text("1").await;
            keys.send(key(KeyCode::Enter)).await;
        })
        .await;

        let calls = fake.calls();
        let at = calls.iter().position(|c| c == "EditForward").unwrap();
        assert_eq!(calls[at + 1], "snapshot", "{calls:?}");
        assert!(matches!(app.mode, Mode::List));
        let message = app.message.as_ref().unwrap();
        assert_eq!(
            (message.text.as_str(), message.error),
            ("saved prd-web", false)
        );
        assert_eq!(app.snapshot.forwards[0].forward.local_port, 10081);
    }

    #[tokio::test(start_paused = true)]
    async fn an_edit_to_another_namespace_keeps_the_cursor_on_its_new_id() {
        let fake = FakeDaemon::new();
        let renamed = ForwardId::new("production.web.frontend-80");
        fake.state().saved_as = Some(renamed.clone());
        let mut app = App::new(snapshot());
        run_loop(&fake, &mut app, async |keys| {
            keys.send(char_key('e')).await;
            for _ in 0..3 {
                keys.send(key(KeyCode::Up)).await;
            }
            keys.send(ctrl('u')).await;
            keys.type_text("web").await;
            keys.send(key(KeyCode::Enter)).await;
        })
        .await;

        assert_eq!(app.snapshot.forwards[0].forward.id, renamed);
        assert_eq!(app.snapshot.forwards[0].forward.namespace, "web");
        assert_eq!(app.selected, Some(renamed));
        let message = app.message.as_ref().unwrap();
        assert_eq!(message.text, "saved production.web.frontend-80");
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
    async fn a_lost_daemon_shows_why_and_is_reconnected() {
        let fake = FakeDaemon::new();
        let mismatch = ClientError::ProtocolMismatch {
            daemon: 3,
            client: 2,
            daemon_version: "2.1.0".into(),
            pid: 4242,
        }
        .to_string();
        {
            let mut state = fake.state();
            state.connected = false;
            state.daemon_back = false;
            state.attach_error = mismatch.clone();
        }
        let mut app = App::new(snapshot());
        let terminal = run_loop(&fake, &mut app, async |keys| keys.ticks(10).await).await;
        // Not the broken connection, but why connecting again fails.
        assert_eq!(app.lost.as_ref(), Some(&mismatch));
        let shown = screen(&terminal);
        let header: Vec<&str> = shown.lines().take(2).map(str::trim).collect();
        assert_eq!(
            header.join(" "),
            format!("lost the daemon: {mismatch}; reconnecting"),
            "{shown}"
        );
        assert!(mismatch.ends_with("restart it with `kprtfwd daemon restart`"));
        // It tries again about once a second, not on every tick.
        let tries = fake.count("reconnect");
        assert!((2..=4).contains(&tries), "{:?}", fake.calls());

        // Back, the daemon's state shows at once and the loss is over.
        {
            let mut state = fake.state();
            state.daemon_back = true;
            state.snapshot.paused = true;
        }
        let terminal = run_loop(&fake, &mut app, async |keys| keys.ticks(1).await).await;
        assert!(app.snapshot.paused);
        assert!(app.lost.is_none());
        assert!(app.message.is_none());
        assert!(screen(&terminal).contains("PAUSED"));
        let calls = fake.calls();
        let back = calls.iter().rposition(|c| c == "reconnect").unwrap();
        assert_eq!(calls[back + 1], "snapshot", "{calls:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn the_old_connection_closes_after_the_new_one_attached() {
        let fake = FakeDaemon::new();
        fake.state().connected = false;
        let mut app = App::new(snapshot());
        run_loop(&fake, &mut app, async |keys| keys.ticks(1).await).await;
        assert!(app.lost.is_none());
        // So the daemon always counted this TUI and kept the forwards on.
        assert_eq!(
            fake.state().links,
            ["open 1", "open 2", "close 1", "close 2"]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn keys_and_ctrl_c_work_while_the_daemon_does_not_answer() {
        let fake = FakeDaemon::new();
        fake.state().stall = vec!["snapshot"];
        let mut app = App::new(snapshot());
        let started = Instant::now();
        run_loop(&fake, &mut app, async |keys| {
            keys.ticks(3).await;
            keys.send(char_key('j')).await;
        })
        .await;
        // ctrl+c ended the loop while the first snapshot was still waiting.
        assert!(started.elapsed() < ANSWER_WITHIN);
        assert_eq!(app.selected, Some(ForwardId::new("stg-db")));
        assert!(app.message.is_none());
        assert_eq!(fake.count("snapshot"), 1, "one call at a time");
        assert_eq!(fake.state().links, ["open 1", "close 1"]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_request_without_answer_loses_the_daemon() {
        let fake = FakeDaemon::new();
        fake.state().stall = vec!["SetEnabled", "reconnect"];
        let mut app = App::new(snapshot());
        run_loop(&fake, &mut app, async |keys| {
            keys.send(char_key(' ')).await;
            keys.send(char_key('j')).await;
            keys.ticks(1).await;
            // Waits for the first request, and is not sent once that fails.
            keys.send(char_key(' ')).await;
            tokio::time::sleep(ANSWER_WITHIN).await;
        })
        .await;
        assert_eq!(app.selected, Some(ForwardId::new("stg-db")));
        assert_eq!(fake.count("SetEnabled"), 1, "{:?}", fake.calls());
        assert_eq!(
            app.lost.as_deref(),
            Some("the daemon did not answer within 5 s")
        );
        let message = app.message.as_ref().unwrap();
        assert_eq!(
            (message.text.as_str(), message.error),
            ("not connected to the daemon", true)
        );
        // The reconnect still waits, and the old connection stays open.
        assert_eq!(fake.count("reconnect"), 1);
        assert_eq!(fake.state().links, ["open 1", "close 1"]);
    }

    #[tokio::test(start_paused = true)]
    async fn keys_work_while_a_reconnect_waits() {
        let fake = FakeDaemon::new();
        {
            let mut state = fake.state();
            state.connected = false;
            state.stall = vec!["reconnect"];
        }
        let mut app = App::new(snapshot());
        run_loop(&fake, &mut app, async |keys| {
            keys.ticks(1).await;
            keys.send(char_key('j')).await;
            tokio::time::sleep(CONNECT_WITHIN + 2 * RECONNECT_EVERY).await;
        })
        .await;
        assert_eq!(app.selected, Some(ForwardId::new("stg-db")));
        // The first attempt gave up after its time limit; the next one waits.
        assert_eq!(fake.count("reconnect"), 2, "{:?}", fake.calls());
        assert_eq!(
            app.lost.as_deref(),
            Some("the daemon did not answer within 15 s")
        );
    }

    #[tokio::test(start_paused = true)]
    async fn commands_fail_at_once_while_the_daemon_is_lost() {
        let fake = FakeDaemon::new();
        {
            let mut state = fake.state();
            state.connected = false;
            state.daemon_back = false;
        }
        let mut app = App::new(snapshot());
        run_loop(&fake, &mut app, async |keys| {
            keys.ticks(1).await;
            keys.send(char_key(' ')).await;
            keys.send(char_key('e')).await;
            keys.send(key(KeyCode::Enter)).await;
        })
        .await;
        assert_eq!(fake.count("SetEnabled"), 0);
        let message = app.message.as_ref().unwrap();
        assert_eq!(
            (message.text.as_str(), message.error),
            ("not connected to the daemon", true)
        );
        // A failed edit keeps its form open.
        assert_eq!(fake.count("EditForward"), 0);
        let Mode::Form(form) = &app.mode else {
            panic!("form closed");
        };
        assert_eq!(form.error.as_deref(), Some("not connected to the daemon"));
    }

    #[tokio::test(start_paused = true)]
    async fn a_discovery_without_answer_ends_after_its_time_limit() {
        // Straight to the command: 6 minutes of ticks would take seconds.
        let fake = FakeDaemon::new();
        let mut link = Link::new(fake.clone(), fake.connect());
        let mut app = App::new(snapshot());
        let (tx, mut rx) = mpsc::unbounded_channel();
        let discover = Cmd::Discover {
            context: "staging".into(),
            namespace_filter: String::new(),
            request: 7,
        };
        let started = Instant::now();
        run_cmd(discover, &mut link, &mut app, &tx);
        let Some(Msg::Discovered {
            request: 7,
            result: Err(text),
        }) = rx.recv().await
        else {
            panic!("no failed discovery");
        };
        assert_eq!(text, "the daemon did not answer within 6 min");
        assert_eq!(started.elapsed(), DISCOVER_WITHIN);
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
    async fn a_discovery_answer_after_esc_is_not_taken_by_the_next_discovery() {
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
            keys.send(key(KeyCode::Esc)).await;
            discover(keys, "web").await;
            // The daemon answers the discovery left with esc first. Taken
            // for web's, it would show shop's services and drop web's.
            fake.discovery.add_permits(1);
            keys.ticks(1).await;
            fake.discovery.add_permits(1);
            keys.ticks(1).await;
        })
        .await;
        assert_eq!(fake.count("aside Discover"), 2);
        let Mode::Discovery(Discovery::Services(services)) = &app.mode else {
            panic!("no services shown");
        };
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

    #[test]
    fn copy_tries_the_next_tool_when_one_is_missing_or_fails() {
        let tools = [
            ("kprtfwd-test-no-such-tool", &[][..]),
            ("sh", &["-c", "echo 'cannot open display' >&2; exit 1"][..]),
            ("sh", &["-c", "test \"$(cat)\" = localhost:10080"][..]),
        ];
        assert_eq!(copy(&tools, "localhost:10080"), Ok(()));

        // When all fail, each one says why.
        let tools = [
            (
                "sh",
                &[
                    "-c",
                    "echo 'cannot open display' >&2; echo more >&2; exit 1",
                ][..],
            ),
            ("kprtfwd-test-no-such-tool", &[][..]),
            ("sh", &["-c", "cat >/dev/null; exit 2"][..]),
        ];
        assert_eq!(
            copy(&tools, "localhost:10080"),
            Err("sh: cannot open display; sh failed (exit status: 2)".into())
        );

        let tools = [("kprtfwd-test-no-such-tool", &[][..])];
        assert_eq!(
            copy(&tools, "x"),
            Err("no clipboard tool found (kprtfwd-test-no-such-tool)".into())
        );
    }

    #[test]
    fn copy_does_not_wait_for_what_a_tool_leaves_behind() {
        // Like wl-copy or xclip: a process keeps stderr open after the tool
        // has ended.
        let started = std::time::Instant::now();
        assert_eq!(
            copy(&[("sh", &["-c", "cat >/dev/null; sleep 2 &"][..])], "x"),
            Ok(())
        );
        let failed = copy(
            &[("sh", &["-c", "sleep 2 & echo oops >&2; exit 1"][..])],
            "x",
        );
        assert_eq!(failed, Err("sh failed (exit status: 1)".into()));
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn open_waits_for_the_opener_and_reports_its_failure() {
        let url = "http://localhost:10080";
        let (tx, mut rx) = mpsc::unbounded_channel();
        assert_eq!(open("true", url, tx), Ok(format!("opened {url}")));
        // The waiting thread ends without a word once the opener is reaped.
        assert!(rx.blocking_recv().is_none());

        let (tx, mut rx) = mpsc::unbounded_channel();
        assert_eq!(open("false", url, tx), Ok(format!("opened {url}")));
        let Some(Msg::Done(Err(text))) = rx.blocking_recv() else {
            panic!("no failure reported");
        };
        assert_eq!(text, format!("false could not open {url} (exit status: 1)"));

        let (tx, _rx) = mpsc::unbounded_channel();
        let missing = open("kprtfwd-test-no-such-opener", url, tx).unwrap_err();
        assert!(
            missing.starts_with("cannot run kprtfwd-test-no-such-opener: "),
            "{missing}"
        );
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
