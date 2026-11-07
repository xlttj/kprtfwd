//! The TUI's state and key handling, without a terminal, so it can be tested.
//!
//! [`App::update`] takes a message (a key, a snapshot, a result) and returns
//! the commands to run: requests to the daemon, opening a URL, and so on.

use std::cell::Cell;
use std::collections::HashSet;

use kprtfwd_core::naming::forward_id;
use kprtfwd_core::svctype::{ServiceType, classify};
use kprtfwd_core::validate::validate_settings;
use kprtfwd_core::{Forward, ForwardId, ForwardSettings, Project, filter};
use kprtfwd_proto::{DiscoveredPort, EventEntry, ForwardView, Request, Snapshot};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

pub enum Msg {
    Key(KeyEvent),
    Snapshot(Snapshot),
    /// A request finished: what to report, or the daemon's error.
    Done(Result<String, String>),
    /// The edit of forward `id` finished: the ID it is saved under, or the
    /// daemon's error.
    Saved {
        id: ForwardId,
        result: Result<ForwardId, String>,
    },
    Contexts(Result<(Vec<String>, Option<String>), String>),
    /// The answer to discovery number `request`.
    Discovered {
        request: u64,
        result: Result<Found, String>,
    },
    Events(Result<Vec<EventEntry>, String>),
    /// The daemon is lost, or still cannot be reached; why.
    Lost(String),
    /// A signal asks the TUI to end.
    Quit,
}

/// What a discovery found.
pub struct Found {
    pub ports: Vec<DiscoveredPort>,
    /// Namespaces whose services the user may not list.
    pub skipped: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Cmd {
    /// Sends a request; `success` is reported when it is done.
    Request {
        request: Request,
        success: String,
    },
    /// Saves the edit of a forward; answered with [`Msg::Saved`].
    Edit {
        id: ForwardId,
        settings: ForwardSettings,
    },
    ListContexts,
    Discover {
        context: String,
        namespace_filter: String,
        /// Comes back with the answer.
        request: u64,
    },
    Events {
        id: ForwardId,
    },
    Open(String),
    Copy(String),
}

pub struct App {
    pub snapshot: Snapshot,
    pub filter: String,
    /// Kept by ID, so the selection survives filtering and list changes.
    pub selected: Option<ForwardId>,
    pub mode: Mode,
    pub message: Option<Message>,
    /// Why the daemon is lost; `None` while connected. The snapshot is then
    /// the last one, not the current state.
    pub lost: Option<String>,
    pub quit: bool,
    /// From the settings, for the help.
    pub keep_forwards_running: bool,
    pub config_path: String,
    /// What the forward list showed last time.
    pub list: Shown,
    /// What the list in a popup showed last time; one popup is open at a
    /// time.
    pub popup: Shown,
    /// The number of the last discovery started.
    discoveries: u64,
}

pub struct Message {
    pub text: String,
    pub error: bool,
}

/// What a scrolling list showed last time; set while drawing. Paging needs
/// the rows, and the list scrolls from where it was, only as far as the
/// selection needs, so the cursor moves within the page.
pub struct Shown {
    pub rows: Cell<usize>,
    /// The first row shown.
    pub top: Cell<usize>,
}

impl Default for Shown {
    fn default() -> Self {
        Self {
            // Until the first drawing says better.
            rows: Cell::new(10),
            top: Cell::new(0),
        }
    }
}

pub enum Mode {
    List,
    /// Typing into the filter.
    Filter,
    Projects {
        selected: usize,
        /// The name of a new project being typed.
        naming: Option<String>,
    },
    Form(Form),
    Discovery(Discovery),
    Events {
        id: ForwardId,
        /// Oldest first.
        entries: Vec<EventEntry>,
        /// Lines scrolled back from the newest event; 0 follows new events.
        back: usize,
    },
    Confirm {
        question: String,
        cmd: Cmd,
    },
    Help,
}

pub const FORM_LABELS: [&str; 5] = [
    "context",
    "namespace",
    "service",
    "remote port",
    "local port",
];

pub struct Form {
    /// The forward being edited; `None` when adding.
    pub editing: Option<Forward>,
    pub fields: [String; 5],
    pub focus: usize,
    pub error: Option<String>,
}

pub enum Discovery {
    /// Waiting for the contexts.
    Loading(String),
    /// Waiting for the answer to discovery number `request`. An answer to
    /// another one belongs to a discovery left with esc and is dropped.
    Listing {
        context: String,
        request: u64,
    },
    Failed(String),
    Contexts {
        names: Vec<String>,
        selected: usize,
    },
    Namespace {
        context: String,
        input: String,
    },
    Services(Services),
}

pub struct Services {
    pub context: String,
    pub ports: Vec<Candidate>,
    /// Namespaces whose services the user may not list.
    pub skipped: Vec<String>,
    pub filter: String,
    pub filtering: bool,
    pub selected: usize,
    /// The local port being typed for the selected entry.
    pub editing: Option<String>,
}

pub struct Candidate {
    pub port: DiscoveredPort,
    pub marked: bool,
}

impl Services {
    /// Indices of the entries the filter lets through.
    pub fn visible(&self) -> Vec<usize> {
        let words: Vec<String> = self
            .filter
            .split_whitespace()
            .map(str::to_lowercase)
            .collect();
        self.ports
            .iter()
            .enumerate()
            .filter(|(_, c)| {
                let text = format!(
                    "{}/{}:{} {}",
                    c.port.namespace,
                    c.port.service,
                    c.port.port,
                    c.port.service_type.label()
                )
                .to_lowercase();
                words.iter().all(|w| text.contains(w.as_str()))
            })
            .map(|(i, _)| i)
            .collect()
    }
}

impl App {
    pub fn new(snapshot: Snapshot) -> Self {
        let mut app = Self {
            snapshot,
            filter: String::new(),
            selected: None,
            mode: Mode::List,
            message: None,
            lost: None,
            quit: false,
            keep_forwards_running: false,
            config_path: String::new(),
            list: Shown::default(),
            popup: Shown::default(),
            discoveries: 0,
        };
        app.keep_selection();
        app
    }

    /// The forwards the filter lets through, in the snapshot's order.
    pub fn visible(&self) -> Vec<&ForwardView> {
        let projects = &self.snapshot.projects;
        self.snapshot
            .forwards
            .iter()
            .filter(|view| {
                filter::matches_with(&self.filter, &view.forward, |name| {
                    projects
                        .iter()
                        .any(|p| p.name == name && p.forwards.contains(&view.forward.id))
                })
            })
            .collect()
    }

    pub fn selected_index(&self) -> Option<usize> {
        let visible = self.visible();
        let id = self.selected.as_ref()?;
        visible.iter().position(|view| &view.forward.id == id)
    }

    fn selected_view(&self) -> Option<&ForwardView> {
        let id = self.selected.as_ref()?;
        self.snapshot
            .forwards
            .iter()
            .find(|view| &view.forward.id == id)
    }

    /// The IDs of the forwards the filter lets through.
    fn listed(&self) -> Vec<ForwardId> {
        self.visible()
            .iter()
            .map(|view| view.forward.id.clone())
            .collect()
    }

    /// Keeps a visible forward selected after the filter changed: the same
    /// one, else the first.
    fn keep_selection(&mut self) {
        self.keep_selection_near(&[]);
    }

    /// Keeps a visible forward selected after the list changed from
    /// `before`: the same one, else its next neighbour in `before` that is
    /// still listed, else the one before it, else the first. So after a
    /// delete the cursor stays where it was.
    fn keep_selection_near(&mut self, before: &[ForwardId]) {
        let listed = self.listed();
        let Some(id) = &self.selected else {
            self.selected = listed.first().cloned();
            return;
        };
        if listed.contains(id) {
            return;
        }
        let near = before.iter().position(|old| old == id).and_then(|at| {
            before[at + 1..]
                .iter()
                .chain(before[..at].iter().rev())
                .find(|old| listed.contains(old))
        });
        self.selected = near.or(listed.first()).cloned();
    }

    fn move_selection(&mut self, delta: isize) {
        let visible = self.visible();
        if visible.is_empty() {
            return;
        }
        let current = self.selected_index().unwrap_or(0) as isize;
        let last = visible.len() as isize - 1;
        let next = (current + delta).clamp(0, last) as usize;
        self.selected = Some(visible[next].forward.id.clone());
    }

    fn info(&mut self, text: impl Into<String>) {
        self.message = Some(Message {
            text: text.into(),
            error: false,
        });
    }

    fn error(&mut self, text: impl Into<String>) {
        self.message = Some(Message {
            text: text.into(),
            error: true,
        });
    }

    /// Commands to run on every refresh tick.
    pub fn tick(&self) -> Vec<Cmd> {
        match &self.mode {
            Mode::Events { id, .. } => vec![Cmd::Events { id: id.clone() }],
            _ => Vec::new(),
        }
    }

    pub fn update(&mut self, msg: Msg) -> Vec<Cmd> {
        match msg {
            Msg::Snapshot(snapshot) => {
                // Back: what was said about the loss is over.
                if self.lost.take().is_some() {
                    self.message = None;
                }
                let before = self.listed();
                self.snapshot = snapshot;
                self.keep_selection_near(&before);
                Vec::new()
            }
            Msg::Lost(reason) => {
                self.lost = Some(reason);
                Vec::new()
            }
            Msg::Quit => {
                self.quit = true;
                Vec::new()
            }
            Msg::Done(result) => {
                self.done(result);
                Vec::new()
            }
            Msg::Saved { id, result } => {
                // The cursor stays on the edited forward.
                if let Ok(saved) = &result
                    && self.selected.as_ref() == Some(&id)
                {
                    self.selected = Some(saved.clone());
                }
                self.done(result.map(|saved| format!("saved {saved}")));
                Vec::new()
            }
            Msg::Contexts(result) => {
                if let Mode::Discovery(step @ Discovery::Loading(_)) = &mut self.mode {
                    *step = match result {
                        Ok((names, current)) if !names.is_empty() => {
                            let selected = current
                                .and_then(|c| names.iter().position(|n| *n == c))
                                .unwrap_or(0);
                            Discovery::Contexts { names, selected }
                        }
                        Ok(_) => Discovery::Failed("the kubeconfig has no contexts".into()),
                        Err(err) => Discovery::Failed(err),
                    };
                }
                Vec::new()
            }
            Msg::Discovered { request, result } => {
                if let Mode::Discovery(step) = &mut self.mode
                    && let Discovery::Listing {
                        context,
                        request: awaited,
                    } = step
                    && *awaited == request
                {
                    let context = std::mem::take(context);
                    *step = match result {
                        Ok(found) => Discovery::Services(Services {
                            context,
                            ports: found
                                .ports
                                .into_iter()
                                .map(|port| Candidate {
                                    port,
                                    marked: false,
                                })
                                .collect(),
                            skipped: found.skipped,
                            filter: String::new(),
                            filtering: false,
                            selected: 0,
                            editing: None,
                        }),
                        Err(err) => Discovery::Failed(err),
                    };
                }
                Vec::new()
            }
            Msg::Events(result) => {
                match (result, &mut self.mode) {
                    (Ok(new), Mode::Events { entries, back, .. }) => {
                        // Scrolled back, the same events stay in view while
                        // newer ones arrive.
                        if *back > 0
                            && let Some(newest) = entries.last()
                            && let Some(at) = new.iter().rposition(|entry| entry == newest)
                        {
                            *back += new.len() - 1 - at;
                        }
                        *entries = new;
                    }
                    (Err(err), Mode::Events { .. }) => {
                        self.mode = Mode::List;
                        self.error(err);
                    }
                    _ => {}
                }
                Vec::new()
            }
            Msg::Key(key) => {
                if ctrl(key, 'c') {
                    self.quit = true;
                    return Vec::new();
                }
                match std::mem::replace(&mut self.mode, Mode::List) {
                    Mode::List => self.list_key(key),
                    Mode::Filter => self.filter_key(key),
                    Mode::Projects { selected, naming } => self.projects_key(key, selected, naming),
                    Mode::Form(form) => self.form_key(key, form),
                    Mode::Discovery(step) => self.discovery_key(key, step),
                    Mode::Events { id, entries, back } => self.events_key(key, id, entries, back),
                    Mode::Confirm { cmd, .. } => {
                        if plain(key) == KeyCode::Char('y') {
                            vec![cmd]
                        } else {
                            Vec::new()
                        }
                    }
                    Mode::Help => Vec::new(),
                }
            }
        }
    }

    /// Reports a finished request. A failed save keeps its form open.
    fn done(&mut self, result: Result<String, String>) {
        match (&mut self.mode, result) {
            (Mode::Form(form), Err(err)) => form.error = Some(err),
            (Mode::Form(_), Ok(text)) => {
                self.mode = Mode::List;
                self.info(text);
            }
            (Mode::Discovery(_), Ok(text)) => {
                self.mode = Mode::List;
                self.info(text);
            }
            (_, Ok(text)) => self.info(text),
            (_, Err(err)) => self.error(err),
        }
    }

    fn list_key(&mut self, key: KeyEvent) -> Vec<Cmd> {
        let page = self.list.rows.get().max(1) as isize;
        match plain(key) {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::PageUp => self.move_selection(-page),
            KeyCode::PageDown => self.move_selection(page),
            KeyCode::Home | KeyCode::Char('g') => self.move_selection(isize::MIN / 2),
            KeyCode::End | KeyCode::Char('G') => self.move_selection(isize::MAX / 2),
            KeyCode::Char('/') => {
                self.mode = Mode::Filter;
                self.message = None;
            }
            KeyCode::Esc => {
                self.filter.clear();
                self.keep_selection();
                self.message = None;
            }
            KeyCode::Char(' ') => {
                if let Some(view) = self.selected_view() {
                    let enabled = !view.forward.enabled;
                    let id = view.forward.id.clone();
                    let success = format!("{} {id}", if enabled { "on:" } else { "off:" });
                    return vec![Cmd::Request {
                        request: Request::SetEnabled {
                            ids: vec![id],
                            enabled,
                        },
                        success,
                    }];
                }
            }
            KeyCode::Char(sign @ ('+' | '-')) => {
                let ids = self.listed();
                if !ids.is_empty() {
                    let enabled = sign == '+';
                    let success = format!(
                        "{} forwards {}",
                        ids.len(),
                        if enabled { "on" } else { "off" }
                    );
                    return vec![Cmd::Request {
                        request: Request::SetEnabled { ids, enabled },
                        success,
                    }];
                }
            }
            KeyCode::Char('p') => {
                self.mode = Mode::Projects {
                    selected: 0,
                    naming: None,
                };
            }
            KeyCode::Char('o') => {
                if let Some(view) = self.selected_view() {
                    let f = &view.forward;
                    if classify(&f.service, f.remote_port) == ServiceType::Http {
                        return vec![Cmd::Open(format!("http://localhost:{}", f.local_port))];
                    }
                    self.error("not an HTTP service; y copies the address");
                }
            }
            KeyCode::Char('y') => {
                if let Some(view) = self.selected_view() {
                    return vec![Cmd::Copy(format!("localhost:{}", view.forward.local_port))];
                }
            }
            KeyCode::Char('e') => {
                if let Some(view) = self.selected_view() {
                    let f = view.forward.clone();
                    self.mode = Mode::Form(Form {
                        fields: [
                            f.context.clone(),
                            f.namespace.clone(),
                            f.service.clone(),
                            f.remote_port.to_string(),
                            f.local_port.to_string(),
                        ],
                        editing: Some(f),
                        focus: 4,
                        error: None,
                    });
                }
            }
            KeyCode::Char('a') => {
                // Most new forwards go next to an existing one.
                let (context, namespace) = self
                    .selected_view()
                    .map(|v| (v.forward.context.clone(), v.forward.namespace.clone()))
                    .unwrap_or_default();
                self.mode = Mode::Form(Form {
                    editing: None,
                    fields: [
                        context,
                        namespace,
                        String::new(),
                        String::new(),
                        String::new(),
                    ],
                    focus: 2,
                    error: None,
                });
            }
            KeyCode::Char('x') => {
                if let Some(view) = self.selected_view() {
                    let id = view.forward.id.clone();
                    self.mode = Mode::Confirm {
                        question: format!("Delete forward {id}?"),
                        cmd: Cmd::Request {
                            success: format!("deleted {id}"),
                            request: Request::DeleteForward { id },
                        },
                    };
                }
            }
            KeyCode::Char('d') => {
                self.mode = Mode::Discovery(Discovery::Loading("contexts".into()));
                return vec![Cmd::ListContexts];
            }
            KeyCode::Char('P') => {
                let paused = !self.snapshot.paused;
                return vec![Cmd::Request {
                    request: Request::SetPaused { paused },
                    success: if paused { "paused" } else { "resumed" }.into(),
                }];
            }
            KeyCode::Char('r') => {
                if let Some(view) = self.selected_view() {
                    let name = view.forward.context.clone();
                    return vec![Cmd::Request {
                        success: format!("reconnected {name}"),
                        request: Request::ReconnectContext { name },
                    }];
                }
            }
            KeyCode::Char('l') => {
                if let Some(view) = self.selected_view() {
                    let id = view.forward.id.clone();
                    self.mode = Mode::Events {
                        id: id.clone(),
                        entries: Vec::new(),
                        back: 0,
                    };
                    return vec![Cmd::Events { id }];
                }
            }
            KeyCode::Char('?') => self.mode = Mode::Help,
            _ => {}
        }
        Vec::new()
    }

    /// Keys in the events view: it scrolls back from the newest event.
    fn events_key(
        &mut self,
        key: KeyEvent,
        id: ForwardId,
        entries: Vec<EventEntry>,
        back: usize,
    ) -> Vec<Cmd> {
        let page = self.popup.rows.get().max(1);
        let oldest = entries.len().saturating_sub(page);
        let back = match plain(key) {
            KeyCode::Esc | KeyCode::Char('q' | 'l') => return Vec::new(),
            KeyCode::Up | KeyCode::Char('k') => back + 1,
            KeyCode::Down | KeyCode::Char('j') => back.saturating_sub(1),
            KeyCode::PageUp => back + page,
            KeyCode::PageDown => back.saturating_sub(page),
            KeyCode::Home | KeyCode::Char('g') => oldest,
            KeyCode::End | KeyCode::Char('G') => 0,
            _ => back,
        };
        self.mode = Mode::Events {
            id,
            entries,
            back: back.min(oldest),
        };
        Vec::new()
    }

    fn filter_key(&mut self, key: KeyEvent) -> Vec<Cmd> {
        match plain(key) {
            KeyCode::Enter => return Vec::new(),
            KeyCode::Esc => self.filter.clear(),
            KeyCode::Up => {
                self.move_selection(-1);
                self.mode = Mode::Filter;
            }
            KeyCode::Down => {
                self.move_selection(1);
                self.mode = Mode::Filter;
            }
            _ => {
                edit(&mut self.filter, key, |_| true);
                self.mode = Mode::Filter;
            }
        }
        self.keep_selection();
        Vec::new()
    }

    fn projects_key(&mut self, key: KeyEvent, selected: usize, naming: Option<String>) -> Vec<Cmd> {
        let listed = self.listed();
        let projects = &self.snapshot.projects;

        if let Some(mut name) = naming {
            match plain(key) {
                KeyCode::Esc => {}
                KeyCode::Enter if !name.trim().is_empty() => {
                    let name = name.trim().to_owned();
                    self.mode = Mode::Projects {
                        selected,
                        naming: None,
                    };
                    return vec![Cmd::Request {
                        success: format!("project {name}: {} forwards", listed.len()),
                        request: Request::UpsertProject {
                            project: Project {
                                name,
                                forwards: listed,
                            },
                        },
                    }];
                }
                _ => {
                    edit(&mut name, key, |_| true);
                    self.mode = Mode::Projects {
                        selected,
                        naming: Some(name),
                    };
                    return Vec::new();
                }
            }
            self.mode = Mode::Projects {
                selected,
                naming: None,
            };
            return Vec::new();
        }

        let last = projects.len().saturating_sub(1);
        let stay = |selected| Mode::Projects {
            selected,
            naming: None,
        };
        let name = projects.get(selected).map(|p| p.name.clone());
        match (plain(key), name) {
            (KeyCode::Esc | KeyCode::Char('q' | 'p'), _) => return Vec::new(),
            (KeyCode::Up | KeyCode::Char('k'), _) => self.mode = stay(selected.saturating_sub(1)),
            (KeyCode::Down | KeyCode::Char('j'), _) => self.mode = stay((selected + 1).min(last)),
            (KeyCode::Char('n'), _) => {
                self.mode = Mode::Projects {
                    selected,
                    naming: Some(String::new()),
                };
            }
            (KeyCode::Enter, Some(name)) => {
                self.mode = stay(selected);
                return vec![Cmd::Request {
                    success: format!("project {name} on"),
                    request: Request::SetProjectEnabled {
                        name,
                        enabled: true,
                    },
                }];
            }
            (KeyCode::Char('-'), Some(name)) => {
                self.mode = stay(selected);
                return vec![Cmd::Request {
                    success: format!("project {name} off"),
                    request: Request::SetProjectEnabled {
                        name,
                        enabled: false,
                    },
                }];
            }
            (KeyCode::Char('s'), Some(name)) => {
                self.mode = stay(selected);
                return vec![Cmd::Request {
                    success: format!("switched to project {name}"),
                    request: Request::SwitchProject { name },
                }];
            }
            (KeyCode::Char('f'), Some(name)) => {
                self.filter = format!("@{name}");
                self.keep_selection();
            }
            (KeyCode::Char('u'), Some(name)) => {
                self.mode = stay(selected);
                return vec![Cmd::Request {
                    success: format!("project {name}: {} forwards", listed.len()),
                    request: Request::UpsertProject {
                        project: Project {
                            name,
                            forwards: listed,
                        },
                    },
                }];
            }
            (KeyCode::Char('x'), Some(name)) => {
                self.mode = Mode::Confirm {
                    question: format!("Delete project {name}? Its forwards stay."),
                    cmd: Cmd::Request {
                        success: format!("deleted project {name}"),
                        request: Request::DeleteProject { name },
                    },
                };
            }
            _ => self.mode = stay(selected),
        }
        Vec::new()
    }

    fn form_key(&mut self, key: KeyEvent, mut form: Form) -> Vec<Cmd> {
        let numeric = form.focus >= 3;
        match plain(key) {
            KeyCode::Esc => return Vec::new(),
            KeyCode::Tab | KeyCode::Down => form.focus = (form.focus + 1) % FORM_LABELS.len(),
            KeyCode::BackTab | KeyCode::Up => {
                form.focus = (form.focus + FORM_LABELS.len() - 1) % FORM_LABELS.len();
            }
            KeyCode::Enter => match self.form_cmd(&form) {
                Ok(cmd) => {
                    form.error = None;
                    self.mode = Mode::Form(form);
                    return vec![cmd];
                }
                Err(err) => form.error = Some(err),
            },
            _ => {
                let field = &mut form.fields[form.focus];
                edit(field, key, |c| !numeric || c.is_ascii_digit());
            }
        }
        self.mode = Mode::Form(form);
        Vec::new()
    }

    /// What saves the form: the edit of the forward it was opened for, or
    /// adding a new one, switched off. Or what is wrong with the form.
    fn form_cmd(&self, form: &Form) -> Result<Cmd, String> {
        let settings = form_settings(form)?;
        if let Some(existing) = &form.editing {
            // Without the enabled flag, which another client may switch
            // while the form is open.
            return Ok(Cmd::Edit {
                id: existing.id.clone(),
                settings,
            });
        }
        let taken: HashSet<ForwardId> = self
            .snapshot
            .forwards
            .iter()
            .map(|v| v.forward.id.clone())
            .collect();
        let ForwardSettings {
            context,
            namespace,
            service,
            remote_port,
            local_port,
        } = settings;
        let forward = Forward {
            id: forward_id(&context, &namespace, &service, remote_port, &taken),
            context,
            namespace,
            service,
            remote_port,
            local_port,
            enabled: false,
        };
        Ok(Cmd::Request {
            success: format!("added {}", forward.id),
            request: Request::AddForwards {
                forwards: vec![forward],
            },
        })
    }

    fn discovery_key(&mut self, key: KeyEvent, step: Discovery) -> Vec<Cmd> {
        let mut keep = |step| self.mode = Mode::Discovery(step);
        match step {
            Discovery::Loading(what) => {
                if plain(key) != KeyCode::Esc {
                    keep(Discovery::Loading(what));
                }
            }
            Discovery::Listing { context, request } => {
                // Esc leaves; the answer will find nothing waiting for it.
                if plain(key) != KeyCode::Esc {
                    keep(Discovery::Listing { context, request });
                }
            }
            Discovery::Failed(_) => {}
            Discovery::Contexts { names, selected } => match plain(key) {
                KeyCode::Esc => {}
                KeyCode::Up | KeyCode::Char('k') => keep(Discovery::Contexts {
                    selected: selected.saturating_sub(1),
                    names,
                }),
                KeyCode::Down | KeyCode::Char('j') => keep(Discovery::Contexts {
                    selected: (selected + 1).min(names.len().saturating_sub(1)),
                    names,
                }),
                KeyCode::Enter => keep(Discovery::Namespace {
                    context: names[selected].clone(),
                    input: String::new(),
                }),
                _ => keep(Discovery::Contexts { names, selected }),
            },
            Discovery::Namespace { context, mut input } => match plain(key) {
                KeyCode::Esc => {}
                KeyCode::Enter => {
                    self.discoveries += 1;
                    let request = self.discoveries;
                    keep(Discovery::Listing {
                        context: context.clone(),
                        request,
                    });
                    return vec![Cmd::Discover {
                        context,
                        namespace_filter: input,
                        request,
                    }];
                }
                _ => {
                    edit(&mut input, key, |_| true);
                    keep(Discovery::Namespace { context, input });
                }
            },
            Discovery::Services(mut services) => {
                let cmds = services_key(key, &mut services);
                if let Some(cmds) = cmds {
                    keep(Discovery::Services(services));
                    return cmds;
                }
            }
        }
        Vec::new()
    }
}

/// The settings a form describes, or what is wrong with them.
fn form_settings(form: &Form) -> Result<ForwardSettings, String> {
    let [context, namespace, service, remote, local] =
        form.fields.clone().map(|f| f.trim().to_owned());
    let port = |label: &str, text: &str| {
        text.parse::<u16>()
            .ok()
            .filter(|p| *p != 0)
            .ok_or_else(|| format!("{label} must be a number from 1 to 65535"))
    };
    let settings = ForwardSettings {
        context,
        namespace,
        service,
        remote_port: port("remote port", &remote)?,
        local_port: port("local port", &local)?,
    };
    validate_settings(&settings).map_err(|err| err.to_string())?;
    Ok(settings)
}

/// Whether `key` is ctrl+`c`.
fn ctrl(key: KeyEvent, c: char) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char(c)
}

/// The key's code when it comes without modifiers but shift. With ctrl,
/// alt or another one it is `Null`, which no action takes: ctrl+q must not
/// quit like q. Only ^H counts, as backspace, which some terminals send.
fn plain(key: KeyEvent) -> KeyCode {
    if key.modifiers.difference(KeyModifiers::SHIFT).is_empty() {
        key.code
    } else if ctrl(key, 'h') {
        KeyCode::Backspace
    } else {
        KeyCode::Null
    }
}

/// Edits a line of text as in a shell: backspace deletes a character,
/// ctrl+w a word, ctrl+u the whole line; plain characters that `accept`
/// takes are typed. Other keys change nothing.
fn edit(line: &mut String, key: KeyEvent, accept: impl Fn(char) -> bool) {
    if ctrl(key, 'w') {
        let kept = line
            .trim_end()
            .trim_end_matches(|c: char| !c.is_whitespace())
            .len();
        line.truncate(kept);
    } else if ctrl(key, 'u') {
        line.clear();
    } else {
        match plain(key) {
            KeyCode::Backspace => {
                line.pop();
            }
            KeyCode::Char(c) if accept(c) => line.push(c),
            _ => {}
        }
    }
}

/// Keys in the service list of discovery. `None` closes discovery.
fn services_key(key: KeyEvent, s: &mut Services) -> Option<Vec<Cmd>> {
    if let Some(mut port) = s.editing.take() {
        match plain(key) {
            KeyCode::Enter => {
                if let Some(index) = s.visible().get(s.selected).copied()
                    && let Ok(local) = port.parse::<u16>()
                    && local != 0
                {
                    s.ports[index].port.local_port = local;
                    s.ports[index].marked = true;
                }
            }
            KeyCode::Esc => {}
            _ => {
                edit(&mut port, key, |c| c.is_ascii_digit());
                s.editing = Some(port);
            }
        }
        return Some(Vec::new());
    }
    if s.filtering {
        match plain(key) {
            KeyCode::Enter => s.filtering = false,
            KeyCode::Esc => {
                s.filter.clear();
                s.filtering = false;
            }
            _ => edit(&mut s.filter, key, |_| true),
        }
        s.selected = 0;
        return Some(Vec::new());
    }

    let visible = s.visible();
    let last = visible.len().saturating_sub(1);
    match plain(key) {
        KeyCode::Esc => return None,
        KeyCode::Up | KeyCode::Char('k') => s.selected = s.selected.saturating_sub(1),
        KeyCode::Down | KeyCode::Char('j') => s.selected = (s.selected + 1).min(last),
        KeyCode::PageUp => s.selected = s.selected.saturating_sub(10),
        KeyCode::PageDown => s.selected = (s.selected + 10).min(last),
        KeyCode::Char('/') => s.filtering = true,
        KeyCode::Char(' ') => {
            if let Some(&index) = visible.get(s.selected)
                && s.ports[index].port.existing.is_none()
            {
                s.ports[index].marked = !s.ports[index].marked;
            }
        }
        KeyCode::Char(sign @ ('+' | '-')) => {
            for index in visible {
                if s.ports[index].port.existing.is_none() {
                    s.ports[index].marked = sign == '+';
                }
            }
        }
        KeyCode::Char('e') => {
            if let Some(&index) = visible.get(s.selected)
                && s.ports[index].port.existing.is_none()
            {
                s.editing = Some(s.ports[index].port.local_port.to_string());
            }
        }
        KeyCode::Enter => {
            let forwards: Vec<Forward> = s
                .ports
                .iter()
                .filter(|c| c.marked && c.port.existing.is_none())
                .map(|c| Forward {
                    id: c.port.id.clone(),
                    context: s.context.clone(),
                    namespace: c.port.namespace.clone(),
                    service: c.port.service.clone(),
                    remote_port: c.port.port,
                    local_port: c.port.local_port,
                    enabled: false,
                })
                .collect();
            if forwards.is_empty() {
                return None;
            }
            return Some(vec![Cmd::Request {
                success: format!("added {} forwards", forwards.len()),
                request: Request::AddForwards { forwards },
            }]);
        }
        _ => {}
    }
    Some(Vec::new())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use kprtfwd_core::ForwardState;
    use kprtfwd_proto::ProjectView;
    use ratatui::crossterm::event::{KeyEventKind, KeyEventState};

    pub fn key(code: KeyCode) -> Msg {
        Msg::Key(KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        })
    }

    fn type_text(app: &mut App, text: &str) -> Vec<Cmd> {
        text.chars()
            .flat_map(|c| app.update(key(KeyCode::Char(c))))
            .collect()
    }

    pub fn view(id: &str, context: &str, service: &str, port: u16, enabled: bool) -> ForwardView {
        ForwardView {
            forward: Forward {
                id: ForwardId::new(id),
                context: context.into(),
                namespace: "shop".into(),
                service: service.into(),
                remote_port: port,
                local_port: 10000 + port,
                enabled,
            },
            state: if enabled {
                ForwardState::Ready {
                    pod: None,
                    active_conns: 0,
                }
            } else {
                ForwardState::Disabled
            },
            last_error: None,
        }
    }

    pub fn snapshot() -> Snapshot {
        Snapshot {
            daemon_version: kprtfwd_proto::VERSION.into(),
            paused: false,
            kubeconfig_files: vec![],
            contexts: vec![],
            forwards: vec![
                view("prd-web", "production", "frontend", 80, false),
                view("stg-db", "staging", "mysql", 3306, false),
                view("stg-web", "staging", "frontend", 80, true),
            ],
            projects: vec![ProjectView {
                name: "web".into(),
                forwards: vec![ForwardId::new("prd-web"), ForwardId::new("stg-web")],
                enabled: 1,
            }],
        }
    }

    fn ids(app: &App) -> Vec<String> {
        app.visible()
            .iter()
            .map(|v| v.forward.id.to_string())
            .collect()
    }

    fn requested(cmds: &[Cmd]) -> Vec<Request> {
        cmds.iter()
            .filter_map(|cmd| match cmd {
                Cmd::Request { request, .. } => Some(request.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn filter_narrows_the_list_and_esc_clears_it() {
        let mut app = App::new(snapshot());
        assert_eq!(app.selected, Some(ForwardId::new("prd-web")));
        app.update(key(KeyCode::Char('/')));
        type_text(&mut app, "stag");
        assert_eq!(ids(&app), ["stg-db", "stg-web"]);
        assert_eq!(
            app.selected,
            Some(ForwardId::new("stg-db")),
            "selection follows the filter"
        );
        app.update(key(KeyCode::Enter));
        assert!(matches!(app.mode, Mode::List));
        assert_eq!(app.filter, "stag");
        app.update(key(KeyCode::Esc));
        assert_eq!(ids(&app).len(), 3);
    }

    #[test]
    fn project_filter_shows_members() {
        let mut app = App::new(snapshot());
        app.filter = "@web stag".into();
        assert_eq!(ids(&app), ["stg-web"]);
    }

    #[test]
    fn space_toggles_and_plus_minus_switch_the_listed_forwards() {
        let mut app = App::new(snapshot());
        app.update(key(KeyCode::Char('j')));
        app.update(key(KeyCode::Char('j')));
        let cmds = app.update(key(KeyCode::Char(' ')));
        assert_eq!(
            requested(&cmds),
            [Request::SetEnabled {
                ids: vec![ForwardId::new("stg-web")],
                enabled: false
            }]
        );

        app.filter = "staging".into();
        let cmds = app.update(key(KeyCode::Char('+')));
        assert_eq!(
            requested(&cmds),
            [Request::SetEnabled {
                ids: vec![ForwardId::new("stg-db"), ForwardId::new("stg-web")],
                enabled: true
            }]
        );
    }

    #[test]
    fn new_project_takes_the_listed_forwards() {
        let mut app = App::new(snapshot());
        app.filter = "frontend".into();
        app.update(key(KeyCode::Char('p')));
        app.update(key(KeyCode::Char('n')));
        type_text(&mut app, "fe");
        let cmds = app.update(key(KeyCode::Enter));
        assert_eq!(
            requested(&cmds),
            [Request::UpsertProject {
                project: Project {
                    name: "fe".into(),
                    forwards: vec![ForwardId::new("prd-web"), ForwardId::new("stg-web")],
                }
            }]
        );
        assert!(matches!(app.mode, Mode::Projects { naming: None, .. }));
    }

    #[test]
    fn add_form_builds_a_forward_and_reports_mistakes() {
        let mut app = App::new(snapshot());
        app.update(key(KeyCode::Char('a')));
        type_text(&mut app, "api");
        app.update(key(KeyCode::Tab));
        type_text(&mut app, "8x0"); // letters are ignored in port fields
        app.update(key(KeyCode::Tab));
        let cmds = app.update(key(KeyCode::Enter));
        assert!(cmds.is_empty());
        let Mode::Form(form) = &app.mode else {
            panic!("form closed")
        };
        assert_eq!(
            form.error.as_deref(),
            Some("local port must be a number from 1 to 65535")
        );

        type_text(&mut app, "18080");
        let cmds = app.update(key(KeyCode::Enter));
        assert_eq!(
            requested(&cmds),
            [Request::AddForwards {
                forwards: vec![Forward {
                    id: ForwardId::new("production.shop.api-80"),
                    context: "production".into(),
                    namespace: "shop".into(),
                    service: "api".into(),
                    remote_port: 80,
                    local_port: 18080,
                    enabled: false,
                }]
            }]
        );
        app.update(Msg::Done(Ok("added".into())));
        assert!(matches!(app.mode, Mode::List));
    }

    #[test]
    fn delete_asks_first() {
        let mut app = App::new(snapshot());
        assert!(app.update(key(KeyCode::Char('x'))).is_empty());
        assert!(app.update(key(KeyCode::Char('n'))).is_empty());
        assert!(matches!(app.mode, Mode::List));
        app.update(key(KeyCode::Char('x')));
        let cmds = app.update(key(KeyCode::Char('y')));
        assert_eq!(
            requested(&cmds),
            [Request::DeleteForward {
                id: ForwardId::new("prd-web")
            }]
        );
    }

    #[test]
    fn discovery_walks_from_context_to_new_forwards() {
        let mut app = App::new(snapshot());
        assert_eq!(app.update(key(KeyCode::Char('d'))), [Cmd::ListContexts]);
        app.update(Msg::Contexts(Ok((
            vec!["production".into(), "staging".into()],
            Some("staging".into()),
        ))));
        app.update(key(KeyCode::Enter));
        type_text(&mut app, "shop");
        assert_eq!(
            app.update(key(KeyCode::Enter)),
            [Cmd::Discover {
                context: "staging".into(),
                namespace_filter: "shop".into(),
                request: 1,
            }]
        );

        let port = |service: &str, existing: bool| DiscoveredPort {
            namespace: "shop".into(),
            service: service.into(),
            port: 80,
            port_name: None,
            service_type: ServiceType::Http,
            existing: existing.then(|| ForwardId::new("old")),
            id: ForwardId::new(format!("staging.shop.{service}-80")),
            local_port: 10080,
        };
        app.update(Msg::Discovered {
            request: 1,
            result: Ok(found(vec![port("api", false), port("frontend", true)])),
        });
        app.update(key(KeyCode::Char('+')));
        let cmds = app.update(key(KeyCode::Enter));
        let requests = requested(&cmds);
        let [Request::AddForwards { forwards }] = requests.as_slice() else {
            panic!("expected one save, got {cmds:?}")
        };
        let added: Vec<&str> = forwards.iter().map(|f| f.id.as_str()).collect();
        assert_eq!(
            added,
            ["staging.shop.api-80"],
            "existing ports are not added again"
        );
    }

    fn message(app: &App) -> (&str, bool) {
        let message = app.message.as_ref().expect("a message");
        (message.text.as_str(), message.error)
    }

    fn listing(context: &str, request: u64) -> Discovery {
        Discovery::Listing {
            context: context.into(),
            request,
        }
    }

    /// Starts a discovery of `namespace` in the context `pick` steps below
    /// the current one; returns its number.
    fn discover(app: &mut App, pick: usize, namespace: &str) -> u64 {
        app.update(key(KeyCode::Char('d')));
        app.update(Msg::Contexts(Ok((
            vec!["production".into(), "staging".into()],
            Some("production".into()),
        ))));
        for _ in 0..pick {
            app.update(key(KeyCode::Down));
        }
        app.update(key(KeyCode::Enter));
        type_text(app, namespace);
        match app.update(key(KeyCode::Enter)).as_slice() {
            [Cmd::Discover { request, .. }] => *request,
            cmds => panic!("no discovery: {cmds:?}"),
        }
    }

    #[test]
    fn a_late_answer_of_a_discovery_left_with_esc_is_dropped() {
        let mut app = App::new(snapshot());
        let first = discover(&mut app, 0, "shop");
        app.update(key(KeyCode::Esc));
        assert!(matches!(app.mode, Mode::List));
        let second = discover(&mut app, 1, "web");
        assert_ne!(first, second);

        // The first discovery answers late, while the second one waits: its
        // services of production must not show up as staging's.
        app.update(Msg::Discovered {
            request: first,
            result: Ok(found(vec![port("api")])),
        });
        let Mode::Discovery(Discovery::Listing { context, request }) = &app.mode else {
            panic!("the late answer was taken");
        };
        assert_eq!((context.as_str(), *request), ("staging", second));
        app.update(Msg::Discovered {
            request: first,
            result: Err("API server unreachable (timeout)".into()),
        });
        assert!(matches!(
            app.mode,
            Mode::Discovery(Discovery::Listing { .. })
        ));

        app.update(Msg::Discovered {
            request: second,
            result: Ok(found(vec![port("frontend")])),
        });
        let Mode::Discovery(Discovery::Services(services)) = &app.mode else {
            panic!("the answer was dropped");
        };
        assert_eq!(services.context, "staging");
        assert_eq!(services.ports[0].port.service, "frontend");

        // The contexts' answer does not replace a waiting discovery either.
        app.update(key(KeyCode::Esc));
        let third = discover(&mut app, 1, "web");
        app.update(Msg::Contexts(Ok((vec!["staging".into()], None))));
        assert!(matches!(
            app.mode,
            Mode::Discovery(Discovery::Listing { request, .. }) if request == third
        ));
    }

    /// `n` events of `prd-web`, a second apart, oldest first.
    pub fn events(n: u64) -> Vec<EventEntry> {
        (0..n)
            .map(|i| EventEntry {
                at: std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(i),
                source: "prd-web".into(),
                message: format!("event {i:03}"),
            })
            .collect()
    }

    pub fn found(ports: Vec<DiscoveredPort>) -> Found {
        Found {
            ports,
            skipped: Vec::new(),
        }
    }

    pub fn port(service: &str) -> DiscoveredPort {
        DiscoveredPort {
            namespace: "shop".into(),
            service: service.into(),
            port: 80,
            port_name: None,
            service_type: ServiceType::Http,
            existing: None,
            id: ForwardId::new(format!("staging.shop.{service}-80")),
            local_port: 10080,
        }
    }

    #[test]
    fn results_outside_their_step_change_nothing() {
        let mut app = App::new(snapshot());
        app.update(Msg::Contexts(Ok((vec!["staging".into()], None))));
        app.update(Msg::Discovered {
            request: 1,
            result: Ok(found(vec![port("api")])),
        });
        app.update(Msg::Events(Ok(vec![])));
        app.update(Msg::Events(Err("late".into())));
        assert!(matches!(app.mode, Mode::List));
        assert!(app.message.is_none());

        // Discovery closed with Esc while loading: its answer is dropped.
        app.update(key(KeyCode::Char('d')));
        app.update(key(KeyCode::Esc));
        app.update(Msg::Contexts(Ok((vec!["staging".into()], None))));
        assert!(matches!(app.mode, Mode::List));
    }

    #[test]
    fn discovery_failures_are_shown_until_a_key() {
        for (result, expected) in [
            (Ok((vec![], None)), "the kubeconfig has no contexts"),
            (
                Err("cannot read the kubeconfig".to_owned()),
                "cannot read the kubeconfig",
            ),
        ] {
            let mut app = App::new(snapshot());
            app.update(key(KeyCode::Char('d')));
            app.update(Msg::Contexts(result));
            let Mode::Discovery(Discovery::Failed(text)) = &app.mode else {
                panic!("not failed");
            };
            assert_eq!(text, expected);
            app.update(key(KeyCode::Char('x')));
            assert!(matches!(app.mode, Mode::List));
        }

        let mut app = App::new(snapshot());
        app.mode = Mode::Discovery(listing("staging", 1));
        app.update(Msg::Discovered {
            request: 1,
            result: Err("API server unreachable (DNS)".into()),
        });
        assert!(matches!(
            &app.mode,
            Mode::Discovery(Discovery::Failed(text)) if text == "API server unreachable (DNS)"
        ));
    }

    #[test]
    fn a_failed_save_keeps_the_form_open() {
        let mut app = App::new(snapshot());
        app.update(key(KeyCode::Char('a')));
        type_text(&mut app, "api");
        app.update(key(KeyCode::Tab));
        type_text(&mut app, "80");
        app.update(key(KeyCode::Tab));
        type_text(&mut app, "18080");
        assert_eq!(requested(&app.update(key(KeyCode::Enter))).len(), 1);

        app.update(Msg::Done(Err("port 18080 is taken".into())));
        let Mode::Form(form) = &app.mode else {
            panic!("form closed")
        };
        assert_eq!(form.error.as_deref(), Some("port 18080 is taken"));
        assert_eq!(form.fields[4], "18080", "the input stays");

        app.update(Msg::Done(Ok("added production.shop.api-80".into())));
        assert!(matches!(app.mode, Mode::List));
        assert_eq!(message(&app), ("added production.shop.api-80", false));
    }

    #[test]
    fn a_saved_edit_closes_the_form_and_a_failed_one_keeps_it() {
        let mut app = App::new(snapshot());
        app.update(key(KeyCode::Char('e')));
        let id = ForwardId::new("prd-web");
        app.update(Msg::Saved {
            id: id.clone(),
            result: Err("unknown forward prd-web".into()),
        });
        let Mode::Form(form) = &app.mode else {
            panic!("form closed")
        };
        assert_eq!(form.error.as_deref(), Some("unknown forward prd-web"));

        app.update(Msg::Saved {
            id: id.clone(),
            result: Ok(id.clone()),
        });
        assert!(matches!(app.mode, Mode::List));
        assert_eq!(message(&app), ("saved prd-web", false));
        assert_eq!(app.selected, Some(id));
    }

    #[test]
    fn a_failed_discovery_save_keeps_the_services_open() {
        let mut app = App::new(snapshot());
        app.mode = Mode::Discovery(listing("staging", 1));
        app.update(Msg::Discovered {
            request: 1,
            result: Ok(found(vec![port("api")])),
        });
        app.update(key(KeyCode::Char(' ')));
        assert_eq!(requested(&app.update(key(KeyCode::Enter))).len(), 1);
        app.update(Msg::Done(Err("port 10080 is taken".into())));
        assert!(matches!(app.mode, Mode::Discovery(Discovery::Services(_))));
        assert_eq!(message(&app), ("port 10080 is taken", true));
        app.update(Msg::Done(Ok("added 1 forwards".into())));
        assert!(matches!(app.mode, Mode::List));
    }

    #[test]
    fn done_in_the_list_reports_both_ways() {
        let mut app = App::new(snapshot());
        app.update(Msg::Done(Ok("paused".into())));
        assert_eq!(message(&app), ("paused", false));
        app.update(Msg::Done(Err("the daemon closed the connection".into())));
        assert_eq!(message(&app), ("the daemon closed the connection", true));
        assert!(matches!(app.mode, Mode::List));
    }

    #[test]
    fn a_lost_daemon_is_kept_until_the_next_snapshot() {
        let mut app = App::new(snapshot());
        app.update(Msg::Lost("the daemon closed the connection".into()));
        app.update(Msg::Done(Err("not connected to the daemon".into())));
        app.update(Msg::Lost("the daemon is not running".into()));
        assert_eq!(app.lost.as_deref(), Some("the daemon is not running"));
        assert_eq!(message(&app), ("not connected to the daemon", true));

        let mut back = snapshot();
        back.paused = true;
        app.update(Msg::Snapshot(back));
        assert!(app.lost.is_none());
        assert!(app.message.is_none(), "the loss is no longer news");
        assert!(app.snapshot.paused);

        // While connected, a snapshot leaves the message alone.
        app.update(Msg::Done(Ok("paused".into())));
        app.update(Msg::Snapshot(snapshot()));
        assert_eq!(message(&app), ("paused", false));
    }

    #[test]
    fn the_events_view_refreshes_until_it_is_closed() {
        let mut app = App::new(snapshot());
        let id = ForwardId::new("prd-web");
        assert_eq!(
            app.update(key(KeyCode::Char('l'))),
            [Cmd::Events { id: id.clone() }]
        );
        assert_eq!(app.tick(), [Cmd::Events { id: id.clone() }]);
        let entry = EventEntry {
            at: std::time::SystemTime::UNIX_EPOCH,
            source: "prd-web".into(),
            message: "pod web-0".into(),
        };
        app.update(Msg::Events(Ok(vec![entry.clone()])));
        app.update(key(KeyCode::Char('j')));
        let Mode::Events { entries, .. } = &app.mode else {
            panic!("events view closed")
        };
        assert_eq!(entries, &[entry]);

        for close in [KeyCode::Esc, KeyCode::Char('q'), KeyCode::Char('l')] {
            app.mode = Mode::Events {
                id: id.clone(),
                entries: vec![],
                back: 0,
            };
            app.update(key(close));
            assert!(matches!(app.mode, Mode::List), "{close:?}");
            assert!(!app.quit);
            assert!(app.tick().is_empty());
        }

        app.update(key(KeyCode::Char('l')));
        app.update(Msg::Events(Err("unknown forward".into())));
        assert!(matches!(app.mode, Mode::List));
        assert_eq!(message(&app), ("unknown forward", true));
    }

    #[test]
    fn the_events_view_scrolls_back_and_keeps_its_place() {
        let back = |app: &App| match &app.mode {
            Mode::Events { back, .. } => *back,
            _ => panic!("events view closed"),
        };
        let mut app = App::new(snapshot());
        app.popup.rows.set(10);
        app.update(key(KeyCode::Char('l')));
        app.update(Msg::Events(Ok(events(30))));
        let mut press = |code| {
            app.update(key(code));
            back(&app)
        };
        assert_eq!(press(KeyCode::Char('k')), 1);
        assert_eq!(press(KeyCode::PageUp), 11);
        assert_eq!(press(KeyCode::Char('g')), 20, "the oldest on top");
        assert_eq!(press(KeyCode::Up), 20);
        assert_eq!(press(KeyCode::PageDown), 10);
        assert_eq!(press(KeyCode::Char('j')), 9);
        assert_eq!(press(KeyCode::Char('G')), 0);
        assert_eq!(press(KeyCode::Down), 0);
        assert_eq!(press(KeyCode::Home), 20);
        assert_eq!(press(KeyCode::End), 0);

        // At the newest it follows new events; scrolled back it stays put,
        // also when old ones drop out.
        app.update(Msg::Events(Ok(events(32))));
        assert_eq!(back(&app), 0);
        app.update(key(KeyCode::Char('k')));
        app.update(Msg::Events(Ok(events(35)[2..].to_vec())));
        assert_eq!(back(&app), 4);
    }

    #[test]
    fn project_keys_act_on_the_selected_project() {
        let mut app = App::new(snapshot());
        app.update(key(KeyCode::Char('p')));
        let mut press = |code| requested(&app.update(key(code)));
        assert_eq!(
            press(KeyCode::Enter),
            [Request::SetProjectEnabled {
                name: "web".into(),
                enabled: true
            }]
        );
        assert_eq!(
            press(KeyCode::Char('-')),
            [Request::SetProjectEnabled {
                name: "web".into(),
                enabled: false
            }]
        );
        assert_eq!(
            press(KeyCode::Char('s')),
            [Request::SwitchProject { name: "web".into() }]
        );
        // There is one project only.
        assert!(press(KeyCode::Char('j')).is_empty());
        assert!(matches!(app.mode, Mode::Projects { selected: 0, .. }));

        // u sets the members to the listed forwards.
        app.filter = "staging".into();
        assert_eq!(
            requested(&app.update(key(KeyCode::Char('u')))),
            [Request::UpsertProject {
                project: Project {
                    name: "web".into(),
                    forwards: vec![ForwardId::new("stg-db"), ForwardId::new("stg-web")],
                }
            }]
        );

        // x asks first.
        assert!(app.update(key(KeyCode::Char('x'))).is_empty());
        assert!(matches!(app.mode, Mode::Confirm { .. }));
        assert_eq!(
            requested(&app.update(key(KeyCode::Char('y')))),
            [Request::DeleteProject { name: "web".into() }]
        );

        // f lists the members and leaves.
        app.filter.clear();
        app.update(key(KeyCode::Char('p')));
        app.update(key(KeyCode::Char('f')));
        assert_eq!(app.filter, "@web");
        assert!(matches!(app.mode, Mode::List));
        assert_eq!(ids(&app), ["prd-web", "stg-web"]);
    }

    #[test]
    fn naming_a_project_can_be_corrected_and_cancelled() {
        let mut app = App::new(snapshot());
        app.update(key(KeyCode::Char('p')));
        app.update(key(KeyCode::Char('n')));
        type_text(&mut app, "apix");
        app.update(key(KeyCode::Backspace));
        assert!(matches!(&app.mode, Mode::Projects { naming: Some(name), .. } if name == "api"));
        // A blank name is not saved.
        let mut blank = App::new(snapshot());
        blank.update(key(KeyCode::Char('p')));
        blank.update(key(KeyCode::Char('n')));
        type_text(&mut blank, "  ");
        assert!(blank.update(key(KeyCode::Enter)).is_empty());

        app.update(key(KeyCode::Esc));
        assert!(matches!(app.mode, Mode::Projects { naming: None, .. }));
        app.update(key(KeyCode::Esc));
        assert!(matches!(app.mode, Mode::List));
    }

    #[test]
    fn the_edit_form_saves_the_settings_without_the_flag() {
        let mut app = App::new(snapshot());
        app.update(key(KeyCode::Char('G')));
        app.update(key(KeyCode::Char('e')));
        let Mode::Form(form) = &app.mode else {
            panic!("no form")
        };
        assert_eq!(form.fields, ["staging", "shop", "frontend", "80", "10080"]);
        assert_eq!(form.focus, 4, "the local port is edited most");

        // Another client switches the forward off while the form is open;
        // the edit carries no flag that could switch it on again.
        let mut switched = snapshot();
        switched.forwards[2].forward.enabled = false;
        app.update(Msg::Snapshot(switched));
        for _ in 0..5 {
            app.update(key(KeyCode::Backspace));
        }
        type_text(&mut app, "18081");
        assert_eq!(
            app.update(key(KeyCode::Enter)),
            [Cmd::Edit {
                id: ForwardId::new("stg-web"),
                settings: ForwardSettings {
                    context: "staging".into(),
                    namespace: "shop".into(),
                    service: "frontend".into(),
                    remote_port: 80,
                    local_port: 18081,
                },
            }]
        );
        assert!(matches!(app.mode, Mode::Form(_)), "open until saved");

        // Focus wraps around; Esc closes without saving.
        app.update(key(KeyCode::Tab));
        assert!(matches!(&app.mode, Mode::Form(form) if form.focus == 0));
        app.update(key(KeyCode::BackTab));
        assert!(matches!(&app.mode, Mode::Form(form) if form.focus == 4));
        assert!(app.update(key(KeyCode::Esc)).is_empty());
        assert!(matches!(app.mode, Mode::List));
    }

    #[test]
    fn paging_keys_move_by_a_page_and_stop_at_the_ends() {
        let mut big = snapshot();
        big.forwards = (0..30u16)
            .map(|i| view(&format!("fwd-{i:02}"), "staging", "api", 8000 + i, false))
            .collect();
        let mut app = App::new(big);
        app.list.rows.set(10);
        let mut at = |code| {
            app.update(key(code));
            app.selected_index().unwrap()
        };
        assert_eq!(at(KeyCode::PageDown), 10);
        assert_eq!(at(KeyCode::PageDown), 20);
        assert_eq!(at(KeyCode::PageDown), 29);
        assert_eq!(at(KeyCode::PageUp), 19);
        assert_eq!(at(KeyCode::Char('g')), 0);
        assert_eq!(at(KeyCode::Up), 0);
        assert_eq!(at(KeyCode::Char('G')), 29);
        assert_eq!(at(KeyCode::Down), 29);
        assert_eq!(at(KeyCode::Home), 0);
        assert_eq!(at(KeyCode::End), 29);
    }

    #[test]
    fn pause_reconnect_open_and_copy_keys() {
        let mut app = App::new(snapshot());
        assert_eq!(
            requested(&app.update(key(KeyCode::Char('P')))),
            [Request::SetPaused { paused: true }]
        );
        app.snapshot.paused = true;
        assert_eq!(
            requested(&app.update(key(KeyCode::Char('P')))),
            [Request::SetPaused { paused: false }]
        );
        assert_eq!(
            requested(&app.update(key(KeyCode::Char('r')))),
            [Request::ReconnectContext {
                name: "production".into()
            }]
        );
        assert_eq!(
            app.update(key(KeyCode::Char('o'))),
            [Cmd::Open("http://localhost:10080".into())]
        );
        assert_eq!(
            app.update(key(KeyCode::Char('y'))),
            [Cmd::Copy("localhost:10080".into())]
        );

        // mysql is not opened in a browser.
        app.update(key(KeyCode::Char('j')));
        assert!(app.update(key(KeyCode::Char('o'))).is_empty());
        assert_eq!(
            message(&app),
            ("not an HTTP service; y copies the address", true)
        );
    }

    #[test]
    fn help_closes_on_any_key() {
        let mut app = App::new(snapshot());
        app.update(key(KeyCode::Char('?')));
        assert!(matches!(app.mode, Mode::Help));
        app.update(key(KeyCode::Char('j')));
        assert!(matches!(app.mode, Mode::List));
        assert_eq!(
            app.selected,
            Some(ForwardId::new("prd-web")),
            "the key is not acted on"
        );
    }

    #[test]
    fn the_selection_follows_its_forward_across_snapshots() {
        let mut app = App::new(snapshot());
        app.update(key(KeyCode::Char('G')));
        assert_eq!(app.selected, Some(ForwardId::new("stg-web")));

        let mut next = snapshot();
        next.forwards.reverse();
        next.forwards
            .insert(0, view("dev-web", "development", "frontend", 80, false));
        app.update(Msg::Snapshot(next));
        assert_eq!(app.selected, Some(ForwardId::new("stg-web")));

        // When it is gone, another listed forward is selected.
        let mut gone = snapshot();
        gone.forwards.retain(|v| v.forward.id.as_str() != "stg-web");
        app.update(Msg::Snapshot(gone));
        let selected = app.selected.clone().unwrap();
        assert!(ids(&app).contains(&selected.to_string()), "{selected}");

        let mut empty = snapshot();
        empty.forwards.clear();
        app.update(Msg::Snapshot(empty));
        assert_eq!(app.selected, None);
    }

    #[test]
    fn a_deleted_forward_hands_the_cursor_to_its_neighbour() {
        let mut app = App::new(snapshot());
        app.update(key(KeyCode::Char('j')));
        let without = |id: &str| {
            let mut next = snapshot();
            next.forwards.retain(|v| v.forward.id.as_str() != id);
            Msg::Snapshot(next)
        };
        app.update(without("stg-db"));
        assert_eq!(
            app.selected,
            Some(ForwardId::new("stg-web")),
            "the next one"
        );

        // The last one hands it to the one before.
        app.update(Msg::Snapshot(snapshot()));
        app.update(without("stg-web"));
        assert_eq!(app.selected, Some(ForwardId::new("stg-db")));

        // Only listed neighbours count.
        app.filter = "web".into();
        app.update(Msg::Snapshot(snapshot()));
        app.update(key(KeyCode::Char('G')));
        assert_eq!(app.selected, Some(ForwardId::new("stg-web")));
        app.update(without("stg-web"));
        assert_eq!(app.selected, Some(ForwardId::new("prd-web")));
    }

    fn chord(code: KeyCode, modifiers: KeyModifiers) -> Msg {
        Msg::Key(KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        })
    }

    fn ctrl_key(c: char) -> Msg {
        chord(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    #[test]
    fn keys_with_ctrl_or_alt_do_nothing_unless_bound() {
        let mut app = App::new(snapshot());
        // ctrl+space arrives as ctrl and a space.
        for c in ['q', ' ', 'd', 'r', 'x', 'j', 'l', 'y', 'o'] {
            assert!(app.update(ctrl_key(c)).is_empty(), "ctrl+{c}");
            assert!(
                app.update(chord(KeyCode::Char(c), KeyModifiers::ALT))
                    .is_empty()
            );
        }
        assert!(
            app.update(chord(KeyCode::Down, KeyModifiers::CONTROL))
                .is_empty()
        );
        assert!(!app.quit, "ctrl+q quit and switched the forwards off");
        assert!(matches!(app.mode, Mode::List));
        assert_eq!(app.selected, Some(ForwardId::new("prd-web")));

        // Shift is part of the key.
        app.update(chord(KeyCode::Char('G'), KeyModifiers::SHIFT));
        assert_eq!(app.selected, Some(ForwardId::new("stg-web")));

        // Only a plain y deletes.
        app.update(key(KeyCode::Char('x')));
        assert!(app.update(ctrl_key('y')).is_empty());
        assert!(matches!(app.mode, Mode::List));

        // ctrl+u is no u among the projects.
        app.update(key(KeyCode::Char('p')));
        assert!(app.update(ctrl_key('u')).is_empty());
        assert!(app.update(ctrl_key('x')).is_empty());
        assert!(matches!(app.mode, Mode::Projects { .. }));
    }

    #[test]
    fn text_inputs_take_backspace_ctrl_h_ctrl_w_and_ctrl_u() {
        let mut app = App::new(snapshot());
        app.update(key(KeyCode::Char('/')));
        type_text(&mut app, "stag  mysql");
        app.update(ctrl_key('w'));
        assert_eq!(app.filter, "stag  ");
        app.update(ctrl_key('w'));
        assert_eq!(app.filter, "");
        type_text(&mut app, "stagx");
        app.update(ctrl_key('h'));
        app.update(chord(KeyCode::Char('x'), KeyModifiers::ALT));
        assert_eq!(app.filter, "stag");
        assert_eq!(ids(&app), ["stg-db", "stg-web"]);
        app.update(ctrl_key('u'));
        assert_eq!(app.filter, "");
        assert!(matches!(app.mode, Mode::Filter));
        app.update(key(KeyCode::Esc));

        // The form: a port field takes digits only, as before.
        app.update(key(KeyCode::Char('a')));
        type_text(&mut app, "api");
        app.update(ctrl_key('h'));
        app.update(key(KeyCode::Tab));
        type_text(&mut app, "8080");
        app.update(ctrl_key('u'));
        app.update(ctrl_key('a'));
        type_text(&mut app, "80");
        let Mode::Form(form) = &app.mode else {
            panic!("form closed")
        };
        assert_eq!(form.fields[2..4], ["ap", "80"]);
        app.update(key(KeyCode::Esc));

        // Naming a project.
        app.update(key(KeyCode::Char('p')));
        app.update(key(KeyCode::Char('n')));
        type_text(&mut app, "web shop");
        app.update(ctrl_key('w'));
        assert!(matches!(&app.mode, Mode::Projects { naming: Some(name), .. } if name == "web "));
        app.update(key(KeyCode::Esc));
        app.update(key(KeyCode::Esc));

        // The namespaces to discover, then the services' filter and port.
        app.update(key(KeyCode::Char('d')));
        app.update(Msg::Contexts(Ok((vec!["staging".into()], None))));
        app.update(key(KeyCode::Enter));
        type_text(&mut app, "shop-*");
        app.update(ctrl_key('u'));
        type_text(&mut app, "shop");
        app.update(ctrl_key('h'));
        assert!(matches!(
            &app.mode,
            Mode::Discovery(Discovery::Namespace { input, .. }) if input == "sho"
        ));
        app.mode = Mode::Discovery(listing("staging", 1));
        app.update(Msg::Discovered {
            request: 1,
            result: Ok(found(vec![port("api"), port("web")])),
        });
        app.update(key(KeyCode::Char('/')));
        type_text(&mut app, "apix");
        app.update(ctrl_key('h'));
        app.update(key(KeyCode::Enter));
        app.update(key(KeyCode::Char('e')));
        app.update(ctrl_key('u'));
        type_text(&mut app, "18080");
        app.update(ctrl_key('h'));
        let Mode::Discovery(Discovery::Services(services)) = &app.mode else {
            panic!("services closed")
        };
        assert_eq!(services.filter, "api");
        assert_eq!(services.editing.as_deref(), Some("1808"));
    }

    #[test]
    fn ctrl_c_quits_from_every_mode() {
        let ctrl_c = Msg::Key(KeyEvent {
            code: KeyCode::Char('c'),
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        });
        let mut app = App::new(snapshot());
        app.update(key(KeyCode::Char('a')));
        assert!(app.update(ctrl_c).is_empty());
        assert!(app.quit);

        // So does a signal.
        let mut app = App::new(snapshot());
        app.update(key(KeyCode::Char('a')));
        assert!(app.update(Msg::Quit).is_empty());
        assert!(app.quit);
    }
}
