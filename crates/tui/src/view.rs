//! Drawing. Colour only in the STATE column (and red for errors), no
//! symbols, no decoration.

use std::time::SystemTime;

use kprtfwd_core::clock::local_time;
use kprtfwd_core::{ContextState, ForwardId, ForwardState};
use kprtfwd_proto::{EventEntry, Snapshot, VERSION};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Row, Table, TableState};

use crate::app::{App, Discovery, FORM_LABELS, Mode, Services, Shown};

const KEYS: &str =
    "space on/off · / filter · +/- all listed · p projects · o open · y copy · ? keys";
/// Errors older than this are not shown next to a ready forward.
const RECENT_ERROR_SECS: u64 = 60;

pub fn draw(frame: &mut Frame, app: &App) {
    // Without the daemon the header says why, on as many lines as it takes:
    // the end of the reason often says what to do.
    let lost = app.lost.as_ref().map(|reason| {
        let text = format!("lost the daemon: {reason}; reconnecting");
        wrap(&text, frame.area().width.saturating_sub(1))
    });
    let header_height = lost
        .as_ref()
        .map_or(1, |lines| u16::try_from(lines.len()).unwrap_or(u16::MAX));
    let [header, filter, list, message, keys] = Layout::vertical([
        Constraint::Length(header_height),
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    match lost {
        Some(lines) => {
            let lines: Vec<Line> = lines
                .into_iter()
                .map(|l| Line::raw(format!(" {l}")))
                .collect();
            frame.render_widget(
                Paragraph::new(lines).style(Style::new().fg(Color::Red)),
                header,
            );
        }
        None => frame.render_widget(header_line(&app.snapshot, header.width), header),
    }
    frame.render_widget(filter_line(app), filter);
    draw_list(frame, app, list);
    if let Some(msg) = &app.message {
        let style = if msg.error {
            Style::new().fg(Color::Red)
        } else {
            Style::new()
        };
        frame.render_widget(Line::styled(format!(" {}", msg.text), style), message);
    }
    frame.render_widget(Line::styled(format!(" {KEYS}"), dim()), keys);

    match &app.mode {
        Mode::List | Mode::Filter => {}
        Mode::Projects { selected, naming } => {
            draw_projects(frame, app, *selected, naming.as_deref())
        }
        Mode::Form(form) => {
            let title = if form.editing.is_some() {
                " edit forward "
            } else {
                " add forward "
            };
            let mut lines: Vec<Line> = FORM_LABELS
                .iter()
                .zip(&form.fields)
                .enumerate()
                .map(|(i, (label, value))| {
                    let cursor = if i == form.focus { "_" } else { "" };
                    let line = format!("{label:>12}  {value}{cursor}");
                    if i == form.focus {
                        Line::styled(line, Style::new().add_modifier(Modifier::BOLD))
                    } else {
                        Line::raw(line)
                    }
                })
                .collect();
            lines.push(Line::raw(""));
            match &form.error {
                Some(err) => lines.push(Line::styled(err.clone(), Style::new().fg(Color::Red))),
                None => lines.push(Line::styled(
                    "tab next field · enter save · esc cancel",
                    dim(),
                )),
            }
            popup(frame, title, lines, 64);
        }
        Mode::Discovery(step) => draw_discovery(frame, app, step),
        Mode::Events { id, entries, back } => draw_events(frame, app, id, entries, *back),
        Mode::Confirm { question, .. } => {
            popup(
                frame,
                " confirm ",
                vec![
                    Line::raw(question.clone()),
                    Line::styled("y yes · any other key no", dim()),
                ],
                70,
            );
        }
        Mode::Help => popup(frame, " keys ", help_lines(app), 70),
    }
}

fn dim() -> Style {
    Style::new().fg(Color::DarkGray)
}

/// Contexts in use, then idle ones, and the version on the right.
fn header_line(snapshot: &Snapshot, width: u16) -> Line<'static> {
    let mut contexts: Vec<(bool, Vec<Span<'static>>)> = snapshot
        .contexts
        .iter()
        .map(|c| {
            let forwards: Vec<_> = snapshot
                .forwards
                .iter()
                .filter(|v| v.forward.context == c.name && v.forward.enabled)
                .collect();
            let ready = forwards
                .iter()
                .filter(|v| matches!(v.state, ForwardState::Ready { .. }))
                .count();
            let style = match &c.state {
                ContextState::Healthy { .. } => Style::new().fg(Color::Green),
                ContextState::Idle | ContextState::Connecting => dim(),
                // The browser may be waiting for the user.
                ContextState::LoggingIn => Style::new().fg(Color::Yellow),
                _ => Style::new().fg(Color::Red),
            };
            let mut text = c.state.summary();
            if !forwards.is_empty() {
                text.push_str(&format!(" {ready}/{}", forwards.len()));
            }
            let idle = c.state == ContextState::Idle;
            (
                idle,
                vec![Span::raw(format!("{} ", c.name)), Span::styled(text, style)],
            )
        })
        .collect();
    contexts.sort_by_key(|(idle, _)| *idle);

    let mut spans = vec![Span::raw(" ")];
    for (i, (_, parts)) in contexts.into_iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" · ", dim()));
        }
        spans.extend(parts);
    }
    if snapshot.paused {
        spans.push(Span::styled(
            "   PAUSED",
            Style::new().fg(Color::Magenta).add_modifier(Modifier::BOLD),
        ));
    }
    // A daemon of another version is served, but runs its own code until
    // it is restarted.
    let version = if snapshot.daemon_version == VERSION {
        Span::styled(format!("kprtfwd {VERSION} "), dim())
    } else {
        Span::styled(
            format!(
                "kprtfwd {VERSION}, daemon {}: kprtfwd daemon restart ",
                snapshot.daemon_version
            ),
            Style::new().fg(Color::Yellow),
        )
    };
    let used: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    let gap = usize::from(width).saturating_sub(used + version.content.chars().count());
    spans.push(Span::raw(" ".repeat(gap)));
    spans.push(version);
    Line::from(spans)
}

/// `text` in lines of at most `width` characters, broken between words.
fn wrap(text: &str, width: u16) -> Vec<String> {
    let width = usize::from(width.max(1));
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split(' ') {
        if !line.is_empty() && line.chars().count() + 1 + word.chars().count() > width {
            lines.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    lines.push(line);
    lines
}

fn filter_line(app: &App) -> Line<'static> {
    match (&app.mode, app.filter.is_empty()) {
        (Mode::Filter, _) => Line::raw(format!(" / {}_", app.filter)),
        (_, true) => Line::styled(" / filter", dim()),
        (_, false) => Line::raw(format!(" / {}", app.filter)),
    }
}

fn state_style(state: &ForwardState) -> Style {
    match state.label() {
        "ready" => Style::new().fg(Color::Green),
        "waiting" => Style::new().fg(Color::Yellow),
        "error" => Style::new().fg(Color::Red),
        "paused" => Style::new().fg(Color::Magenta),
        _ => dim(),
    }
}

/// The first of `len` rows to show in `rows` rows, so that `selected` is in
/// view: `shown`'s first row from last time, moved only as far as needed,
/// and so that the rows are filled. Notes it and the rows in `shown`.
fn scroll(shown: &Shown, selected: Option<usize>, len: usize, rows: usize) -> usize {
    let mut top = shown.top.get();
    if let Some(selected) = selected {
        top = top
            .min(selected)
            .max((selected + 1).saturating_sub(rows.max(1)));
    }
    let top = top.min(len.saturating_sub(rows));
    shown.top.set(top);
    shown.rows.set(rows);
    top
}

fn draw_list(frame: &mut Frame, app: &App, area: Rect) {
    let visible = app.visible();
    // The header takes a row.
    let rows = usize::from(area.height.saturating_sub(1));
    let selected = app.selected_index();
    let top = scroll(&app.list, selected, visible.len(), rows);
    if visible.is_empty() {
        let text = if app.snapshot.forwards.is_empty() {
            " No forwards yet. d discovers services, a adds one."
        } else {
            " Nothing matches the filter. esc clears it."
        };
        frame.render_widget(Paragraph::new(Line::styled(text, dim())), area);
        return;
    }

    // Without the daemon the states are the last known ones: all grey.
    let stale = app.lost.is_some();
    let rows: Vec<Row> = visible
        .iter()
        .map(|view| {
            let f = &view.forward;
            let context = app.snapshot.context_state(&f.context);
            let state = if stale {
                dim()
            } else {
                state_style(&view.state)
            };
            Row::new(vec![
                Span::styled(view.state.label(), state),
                Span::raw(f.context.clone()),
                Span::raw(format!("{}/{}:{}", f.namespace, f.service, f.remote_port)),
                Span::raw(f.local_port.to_string()),
                Span::raw(info(view, &context)),
            ])
            .style(if stale { dim() } else { Style::new() })
        })
        .collect();
    let width = |values: Vec<usize>, title: &str| {
        let widest = values.into_iter().max().unwrap_or(0).max(title.len());
        Constraint::Length(u16::try_from(widest).unwrap_or(u16::MAX))
    };
    let widths = [
        Constraint::Length(7),
        width(
            visible
                .iter()
                .map(|v| v.forward.context.chars().count())
                .collect(),
            "CONTEXT",
        ),
        width(
            visible
                .iter()
                .map(|v| {
                    let f = &v.forward;
                    format!("{}/{}:{}", f.namespace, f.service, f.remote_port)
                        .chars()
                        .count()
                })
                .collect(),
            "SERVICE",
        ),
        Constraint::Length(5),
        Constraint::Fill(1),
    ];
    let table = Table::new(rows, widths)
        .header(Row::new(["STATE", "CONTEXT", "SERVICE", "LOCAL", "INFO"]).style(dim()))
        .column_spacing(2)
        .row_highlight_style(Style::new().add_modifier(Modifier::REVERSED));
    let mut state = TableState::new().with_offset(top).with_selected(selected);
    let inner = Rect {
        x: area.x + 1,
        width: area.width.saturating_sub(1),
        ..area
    };
    frame.render_stateful_widget(table, inner, &mut state);
}

/// Why a forward is not ready, or its last error if that is recent.
fn info(view: &kprtfwd_proto::ForwardView, context: &ContextState) -> String {
    let text = view.state.info(context);
    if !text.is_empty() {
        return text;
    }
    match &view.last_error {
        Some(err)
            if SystemTime::now()
                .duration_since(err.at)
                .is_ok_and(|age| age.as_secs() < RECENT_ERROR_SECS) =>
        {
            err.message.clone()
        }
        _ => String::new(),
    }
}

/// The events oldest first, so the newest are at the bottom, next to the
/// keys; scrolled to the newest unless the user scrolled back.
fn draw_events(frame: &mut Frame, app: &App, id: &ForwardId, entries: &[EventEntry], back: usize) {
    let mut items: Vec<Line> = entries
        .iter()
        .map(|e| {
            // Context events say which context; the forward's own don't need to.
            if e.source == id.as_str() {
                Line::raw(format!("{}  {}", local_time(e.at), e.message))
            } else {
                Line::raw(format!("{}  {}: {}", local_time(e.at), e.source, e.message))
            }
        })
        .collect();
    if items.is_empty() {
        items.push(Line::styled("no events yet", dim()));
    }
    // A blank line and the keys below the events.
    let area = popup_area(frame, 100, items.len() + 2);
    let rows = usize::from(area.height).saturating_sub(4);
    app.popup.rows.set(rows);
    let oldest = items.len().saturating_sub(rows);
    let back = back.min(oldest);
    let keys = match (oldest, back) {
        (0, _) => "esc close".to_owned(),
        (_, 0) => "j k PgUp PgDn g G scroll · esc close".to_owned(),
        (_, back) => format!("{back} newer below · j k PgUp PgDn g G scroll · esc close"),
    };
    let lines = items
        .into_iter()
        .skip(oldest - back)
        .take(rows)
        .chain([Line::raw(""), Line::styled(keys, dim())])
        .collect();
    boxed(frame, area, &format!(" events: {id} "), lines);
}

fn draw_projects(frame: &mut Frame, app: &App, selected: usize, naming: Option<&str>) {
    let mut items: Vec<Line> = app
        .snapshot
        .projects
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let text = format!("{:<24} {}/{}", p.name, p.enabled, p.forwards.len());
            if i == selected && naming.is_none() {
                Line::styled(text, Style::new().add_modifier(Modifier::REVERSED))
            } else {
                Line::raw(text)
            }
        })
        .collect();
    if items.is_empty() {
        items.push(Line::styled("no projects yet", dim()));
    }
    let mut footer = vec![Line::raw("")];
    match naming {
        Some(name) => {
            let listed = app.visible().len();
            footer.push(Line::raw(format!(
                "new project with the {listed} listed forwards: {name}_"
            )));
            footer.push(Line::styled("enter create · esc cancel", dim()));
        }
        None => {
            footer.push(Line::styled(
                "enter on · - off · s switch to · f show members",
                dim(),
            ));
            footer.push(Line::styled(
                "n new from list · u set to list · x delete · esc back",
                dim(),
            ));
        }
    }
    scrolled_popup(frame, " projects ", items, &app.popup, selected, footer, 64);
}

fn draw_discovery(frame: &mut Frame, app: &App, step: &Discovery) {
    match step {
        Discovery::Loading(what) | Discovery::Listing { context: what, .. } => popup(
            frame,
            " discover ",
            vec![
                Line::raw(format!("loading {what} ...")),
                Line::styled("esc cancel", dim()),
            ],
            60,
        ),
        Discovery::Failed(err) => popup(
            frame,
            " discover ",
            vec![
                Line::styled(err.clone(), Style::new().fg(Color::Red)),
                Line::styled("any key back", dim()),
            ],
            80,
        ),
        Discovery::Contexts { names, selected } => {
            let items: Vec<Line> = names
                .iter()
                .enumerate()
                .map(|(i, name)| {
                    if i == *selected {
                        Line::styled(name.clone(), Style::new().add_modifier(Modifier::REVERSED))
                    } else {
                        Line::raw(name.clone())
                    }
                })
                .collect();
            let footer = vec![
                Line::raw(""),
                Line::styled("enter choose · esc cancel", dim()),
            ];
            let title = " discover: context ";
            scrolled_popup(frame, title, items, &app.popup, *selected, footer, 60);
        }
        Discovery::Namespace { context, input } => popup(
            frame,
            &format!(" discover: {context} "),
            vec![
                Line::raw(format!("namespaces: {input}_")),
                Line::styled(
                    "empty for all, * as wildcard, e.g. shop-* · enter list · esc cancel",
                    dim(),
                ),
            ],
            80,
        ),
        Discovery::Services(services) => draw_services(frame, &app.popup, services),
    }
}

fn draw_services(frame: &mut Frame, shown: &Shown, s: &Services) {
    let area = centered(frame.area(), 110, frame.area().height.saturating_sub(4));
    frame.render_widget(Clear, area);
    let block = Block::bordered().title(format!(" discover: {} ", s.context));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let [filter, list, skipped, keys] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(u16::from(!s.skipped.is_empty())),
        Constraint::Length(1),
    ])
    .areas(inner);

    let filter_text = if s.filtering {
        format!("/ {}_", s.filter)
    } else if s.filter.is_empty() {
        "/ filter".into()
    } else {
        format!("/ {}", s.filter)
    };
    frame.render_widget(
        Line::styled(filter_text, if s.filtering { Style::new() } else { dim() }),
        filter,
    );

    let visible = s.visible();
    let rows: Vec<Row> = visible
        .iter()
        .enumerate()
        .map(|(row, &index)| {
            let c = &s.ports[index];
            let mark = match (c.port.existing.is_some(), c.marked) {
                (true, _) => "added",
                (false, true) => "[x]",
                (false, false) => "[ ]",
            };
            let local = match &s.editing {
                Some(text) if row == s.selected => format!("{text}_"),
                _ => c.port.local_port.to_string(),
            };
            let style = if c.port.existing.is_some() {
                dim()
            } else {
                Style::new()
            };
            Row::new(vec![
                mark.to_owned(),
                format!("{}/{}:{}", c.port.namespace, c.port.service, c.port.port),
                c.port.service_type.label().to_owned(),
                local,
            ])
            .style(style)
        })
        .collect();
    let table = Table::new(
        rows,
        [
            Constraint::Length(5),
            Constraint::Fill(1),
            Constraint::Length(14),
            Constraint::Length(6),
        ],
    )
    .header(Row::new(["", "SERVICE", "TYPE", "LOCAL"]).style(dim()))
    .column_spacing(2)
    .row_highlight_style(Style::new().add_modifier(Modifier::REVERSED));
    let selected = (!visible.is_empty()).then_some(s.selected);
    let rows = usize::from(list.height.saturating_sub(1));
    let top = scroll(shown, selected, visible.len(), rows);
    let mut state = TableState::new().with_offset(top).with_selected(selected);
    frame.render_stateful_widget(table, list, &mut state);

    if let Some(note) = skipped_note(&s.skipped) {
        frame.render_widget(Line::styled(note, Style::new().fg(Color::Yellow)), skipped);
    }
    let marked = s.ports.iter().filter(|c| c.marked).count();
    frame.render_widget(
        Line::styled(format!("{marked} marked · space mark · +/- all listed · e local port · / filter · enter add · esc cancel"), dim()),
        keys,
    );
}

/// Names the namespaces discovery skipped; the count comes first, so it
/// shows even when the names are cut off.
fn skipped_note(skipped: &[String]) -> Option<String> {
    match skipped {
        [] => None,
        [namespace] => Some(format!(
            "namespace {namespace} skipped: you may not list its services"
        )),
        _ => Some(format!(
            "{} namespaces skipped, you may not list their services: {}",
            skipped.len(),
            skipped.join(", ")
        )),
    }
}

fn help_lines(app: &App) -> Vec<Line<'static>> {
    let quit = if app.keep_forwards_running {
        "q                        quit; forwards keep running"
    } else {
        "q                        quit and switch all forwards off"
    };
    let mut lines: Vec<Line<'static>> = [
        "↑↓ j k  PgUp PgDn  g G   move",
        "space                    selected forward on/off",
        "/                        filter: all words must match; @name = project",
        "esc                      clear the filter",
        "+ -                      all listed forwards on / off",
        "p                        projects",
        "o  y                     open in browser / copy localhost:port",
        "e  a  x                  edit / add / delete forward",
        "d                        discover services",
        "P                        pause or resume all",
        "r                        reconnect the selected forward's context",
        "l                        events of the selected forward",
        "ctrl+l                   redraw the screen",
        "ctrl+w  ctrl+u           in a text input: delete a word / everything",
        quit,
    ]
    .into_iter()
    .map(Line::raw)
    .collect();
    if !app.config_path.is_empty() {
        lines.push(Line::styled(
            format!("settings: {}", app.config_path),
            dim(),
        ));
    }
    lines
}

/// A bordered box in the middle of the screen, sized to its lines.
fn popup(frame: &mut Frame, title: &str, lines: Vec<Line>, width: u16) {
    let area = popup_area(frame, width, lines.len());
    boxed(frame, area, title, lines);
}

/// A popup of `items` that keeps the `selected` one in view when they do
/// not fit the screen; it scrolls from where it was last time (`shown`).
/// `footer` stays below the items.
fn scrolled_popup(
    frame: &mut Frame,
    title: &str,
    items: Vec<Line>,
    shown: &Shown,
    selected: usize,
    footer: Vec<Line>,
    width: u16,
) {
    let area = popup_area(frame, width, items.len() + footer.len());
    let rows = usize::from(area.height).saturating_sub(footer.len() + 2);
    let top = scroll(shown, Some(selected), items.len(), rows);
    let lines = items
        .into_iter()
        .skip(top)
        .take(rows)
        .chain(footer)
        .collect();
    boxed(frame, area, title, lines);
}

/// Where a popup of `lines` lines goes: in the middle, cut to the screen.
fn popup_area(frame: &Frame, width: u16, lines: usize) -> Rect {
    let height = u16::try_from(lines).unwrap_or(u16::MAX).saturating_add(2);
    centered(frame.area(), width, height)
}

fn boxed(frame: &mut Frame, area: Rect, title: &str, lines: Vec<Line>) {
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines).block(Block::bordered().title(title.to_owned())),
        area,
    );
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::tests::{events, key, port, snapshot, view};
    use crate::app::{Candidate, Msg};
    use kprtfwd_proto::ContextView;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::KeyCode;

    fn render(app: &App, width: u16, height: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| draw(frame, app)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect()
    }

    #[test]
    fn a_daemon_of_another_version_shows_how_to_restart_it() {
        let mut snap = snapshot();
        snap.daemon_version = "1.9.0".into();
        let app = App::new(snap);
        let mut terminal = Terminal::new(TestBackend::new(100, 6)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let buffer = terminal.backend().buffer();
        let header: String = (0..100).map(|x| buffer[(x, 0)].symbol()).collect();
        let hint = format!("kprtfwd {VERSION}, daemon 1.9.0: kprtfwd daemon restart");
        assert!(header.trim_end().ends_with(&hint), "{header}");
        let at = header.find(&hint).unwrap() as u16;
        assert_eq!(buffer[(at, 0)].fg, Color::Yellow);
    }

    #[test]
    fn shows_contexts_list_and_reasons() {
        let mut snap = snapshot();
        snap.contexts = vec![
            ContextView {
                name: "production".into(),
                state: ContextState::Idle,
            },
            ContextView {
                name: "staging".into(),
                state: ContextState::Unreachable {
                    since: SystemTime::UNIX_EPOCH,
                    cause: kprtfwd_core::state::NetCause::Dns,
                },
            },
        ];
        snap.forwards[2].state = ForwardState::WaitingForContext;
        let app = App::new(snap);
        let screen = render(&app, 100, 8);

        assert!(
            screen[0].starts_with(" staging unreachable (DNS) 0/1 · production idle"),
            "{}",
            screen[0]
        );
        assert!(
            screen[0].ends_with(&format!("kprtfwd {VERSION}")),
            "{}",
            screen[0]
        );
        assert_eq!(screen[1], " / filter");
        assert!(
            screen[2].contains("STATE") && screen[2].contains("INFO"),
            "{}",
            screen[2]
        );
        assert!(
            screen[3].contains("off") && screen[3].contains("shop/frontend:80"),
            "{}",
            screen[3]
        );
        let waiting = screen.iter().find(|line| line.contains("waiting")).unwrap();
        assert!(waiting.contains("API unreachable (DNS)"), "{waiting}");
        assert!(screen[7].contains("space on/off"), "{}", screen[7]);
    }

    #[test]
    fn a_lost_daemon_greys_the_list_and_says_why() {
        let mut app = App::new(snapshot());
        let screen = render(&app, 100, 8);
        let (x, y) = position(&screen, "ready");
        assert_eq!(cell_fg(&app, x, y), Color::Green);

        app.lost = Some("restart it with `kprtfwd daemon restart`".into());
        let screen = render(&app, 100, 8);
        assert_eq!(
            screen[0],
            " lost the daemon: restart it with `kprtfwd daemon restart`; reconnecting"
        );
        assert_eq!(cell_fg(&app, 1, 0), Color::Red);
        // The forwards are still listed, with their last states in grey.
        assert_eq!(position(&screen, "ready"), (x, y));
        assert_eq!(cell_fg(&app, x, y), Color::DarkGray);
        assert_eq!(cell_fg(&app, x + 10, y), Color::DarkGray, "the context too");

        // A long reason takes more lines, so its end is not cut off.
        app.lost = Some(
            "a daemon is running that this kprtfwd cannot talk to (probably another version); restart it with `kprtfwd daemon restart`".into(),
        );
        let screen = render(&app, 100, 10);
        assert_eq!(
            screen[..2],
            [
                " lost the daemon: a daemon is running that this kprtfwd cannot talk to (probably another version);",
                " restart it with `kprtfwd daemon restart`; reconnecting",
            ]
        );
        assert_eq!(screen[2], " / filter");
    }

    #[test]
    fn wrap_breaks_between_words() {
        assert_eq!(wrap("one two three", 7), ["one two", "three"]);
        assert_eq!(wrap("one two three", 20), ["one two three"]);
        assert_eq!(wrap("a-very-long-word x", 4), ["a-very-long-word", "x"]);
    }

    /// Column and row of `text` on the screen.
    fn position(screen: &[String], text: &str) -> (u16, u16) {
        let y = screen.iter().position(|line| line.contains(text)).unwrap();
        let x = screen[y].find(text).unwrap();
        (x as u16, y as u16)
    }

    fn cell_fg(app: &App, x: u16, y: u16) -> Color {
        let mut terminal = Terminal::new(TestBackend::new(100, 8)).unwrap();
        terminal.draw(|frame| draw(frame, app)).unwrap();
        terminal.backend().buffer()[(x, y)].fg
    }

    #[test]
    fn help_says_what_quitting_does() {
        let mut app = App::new(snapshot());
        app.mode = Mode::Help;
        app.config_path = "~/.config/kprtfwd/config.toml".into();
        let screen = render(&app, 100, 24).join("\n");
        assert!(
            screen.contains("quit and switch all forwards off"),
            "{screen}"
        );
        assert!(
            screen.contains("settings: ~/.config/kprtfwd/config.toml"),
            "{screen}"
        );

        app.keep_forwards_running = true;
        let screen = render(&app, 100, 24).join("\n");
        assert!(screen.contains("quit; forwards keep running"), "{screen}");
    }

    #[test]
    fn discovery_names_the_namespaces_it_skipped() {
        let services = |skipped: &[&str]| Services {
            context: "staging".into(),
            ports: vec![Candidate {
                port: port("api"),
                marked: false,
            }],
            skipped: skipped.iter().map(|ns| (*ns).to_owned()).collect(),
            filter: String::new(),
            filtering: false,
            selected: 0,
            editing: None,
        };
        let mut app = App::new(snapshot());
        app.mode = Mode::Discovery(Discovery::Services(services(&[])));
        let screen = render(&app, 120, 12).join("\n");
        assert!(screen.contains("shop/api:80"), "{screen}");
        assert!(!screen.contains("skipped"), "{screen}");

        app.mode = Mode::Discovery(Discovery::Services(services(&["kube-system", "web"])));
        let screen = render(&app, 120, 12);
        let note = screen
            .iter()
            .position(|line| {
                line.contains(
                    "2 namespaces skipped, you may not list their services: kube-system, web",
                )
            })
            .expect("no note");
        assert!(screen[note + 1].contains("space mark"), "{screen:?}");
        assert!(screen.iter().any(|line| line.contains("shop/api:80")));

        app.mode = Mode::Discovery(Discovery::Services(services(&["web"])));
        let screen = render(&app, 120, 12).join("\n");
        assert!(
            screen.contains("namespace web skipped: you may not list its services"),
            "{screen}"
        );
    }

    /// `n` forwards, `fwd-00` on, all with their own port.
    fn many(n: u16) -> Snapshot {
        let mut snap = snapshot();
        snap.forwards = (0..n)
            .map(|i| view(&format!("fwd-{i:02}"), "staging", "api", 8000 + i, false))
            .collect();
        snap
    }

    /// The screen row that shows forward number `i` of [`many`].
    fn row_of(screen: &[String], i: u16) -> Option<usize> {
        let service = format!("shop/api:{}", 8000 + i);
        screen.iter().position(|line| line.contains(&service))
    }

    #[test]
    fn the_list_scrolls_only_when_the_cursor_leaves_the_page() {
        let mut app = App::new(many(30));
        // 12 lines leave 7 rows for forwards, from line 3 to 9.
        let press = |app: &mut App, code, times| {
            for _ in 0..times {
                app.update(key(code));
            }
            render(app, 100, 12)
        };
        let screen = press(&mut app, KeyCode::Char('j'), 10);
        assert_eq!(app.list.rows.get(), 7);
        assert_eq!(
            (row_of(&screen, 4), row_of(&screen, 10)),
            (Some(3), Some(9))
        );

        // Up within the page moves the cursor, not the list.
        let screen = press(&mut app, KeyCode::Char('k'), 3);
        assert_eq!((row_of(&screen, 4), row_of(&screen, 7)), (Some(3), Some(6)));
        let screen = press(&mut app, KeyCode::Char('k'), 4);
        assert_eq!(row_of(&screen, 3), Some(3), "scrolled once the cursor left");

        // A deleted forward leaves the cursor where it was, on its neighbour.
        let screen = press(&mut app, KeyCode::Char('j'), 4);
        assert_eq!(row_of(&screen, 7), Some(7));
        let mut gone = many(30);
        gone.forwards.remove(7);
        app.update(Msg::Snapshot(gone));
        let screen = render(&app, 100, 12);
        assert_eq!(app.selected, Some(ForwardId::new("fwd-08")));
        assert_eq!((row_of(&screen, 3), row_of(&screen, 8)), (Some(3), Some(7)));

        // At the end the list fills the page, also after it got shorter.
        let screen = press(&mut app, KeyCode::Char('G'), 1);
        assert_eq!(
            (row_of(&screen, 23), row_of(&screen, 29)),
            (Some(3), Some(9))
        );
        app.update(Msg::Snapshot(many(25)));
        let screen = render(&app, 100, 12);
        assert_eq!(
            (row_of(&screen, 18), row_of(&screen, 24)),
            (Some(3), Some(9))
        );
    }

    #[test]
    fn scroll_moves_only_as_far_as_the_selection_needs() {
        let shown = Shown::default();
        let at = |selected, len, rows| scroll(&shown, selected, len, rows);
        assert_eq!(at(Some(3), 30, 10), 0);
        assert_eq!(
            at(Some(12), 30, 10),
            3,
            "down: the selection on the last row"
        );
        assert_eq!(at(Some(5), 30, 10), 3, "within the page: stays");
        assert_eq!(at(Some(1), 30, 10), 1, "up: the selection on the first row");
        assert_eq!(at(Some(29), 30, 10), 20);
        assert_eq!(at(Some(14), 15, 10), 5, "shorter: the rows stay filled");
        assert_eq!(at(None, 8, 10), 0);
        assert_eq!(at(Some(0), 1, 0), 0, "no rows");
        assert_eq!(shown.rows.get(), 0);
    }

    #[test]
    fn the_events_view_shows_the_newest_and_scrolls_back() {
        let mut app = App::new(snapshot());
        app.update(key(KeyCode::Char('l')));
        app.update(Msg::Events(Ok(events(60))));
        let screen = render(&app, 120, 30).join("\n");
        // 30 lines leave 26 for events, after the border, the blank line
        // and the keys.
        assert_eq!(app.popup.rows.get(), 26);
        assert!(screen.contains("event 059"), "{screen}");
        assert!(screen.contains("event 034"), "{screen}");
        assert!(!screen.contains("event 033"), "{screen}");
        assert!(screen.contains("j k PgUp PgDn g G scroll · esc close"));

        app.update(key(KeyCode::Char('k')));
        let screen = render(&app, 120, 30).join("\n");
        assert!(screen.contains("event 033") && !screen.contains("event 059"));
        assert!(screen.contains("1 newer below"), "{screen}");

        app.update(key(KeyCode::Char('g')));
        let screen = render(&app, 120, 30).join("\n");
        assert!(screen.contains("event 000") && screen.contains("event 025"));
        assert!(screen.contains("34 newer below"), "{screen}");

        // A short list shows whole, with nothing to scroll.
        app.update(Msg::Events(Ok(events(3))));
        let screen = render(&app, 120, 30).join("\n");
        assert!(screen.contains("event 000") && screen.contains("event 002"));
        assert!(screen.contains("esc close") && !screen.contains("scroll"));
    }

    #[test]
    fn popups_keep_the_selection_in_view() {
        let mut app = App::new(snapshot());
        let names: Vec<String> = (0..40).map(|i| format!("cluster-{i:02}")).collect();
        app.mode = Mode::Discovery(Discovery::Contexts {
            names,
            selected: 35,
        });
        let screen = render(&app, 100, 20);
        let at = position(&screen, "cluster-35").1;
        let below = position(&screen, "enter choose").1;
        assert!(at < below, "{screen:?}");
        assert!(screen.iter().any(|line| line.contains("cluster-26")));

        app.snapshot.projects = (0..30)
            .map(|i| kprtfwd_proto::ProjectView {
                name: format!("project-{i:02}"),
                forwards: Vec::new(),
                enabled: 0,
            })
            .collect();
        app.mode = Mode::Projects {
            selected: 29,
            naming: None,
        };
        let screen = render(&app, 100, 20);
        assert!(position(&screen, "project-29").1 < position(&screen, "x delete").1);
        assert!(!screen.iter().any(|line| line.contains("project-00")));
    }

    #[test]
    fn says_how_to_start_when_empty() {
        let mut snap = snapshot();
        snap.forwards.clear();
        let screen = render(&App::new(snap), 80, 6);
        assert!(screen[2].contains("No forwards yet"), "{}", screen[2]);

        let mut app = App::new(snapshot());
        app.filter = "nothing-like-this".into();
        let screen = render(&app, 80, 6);
        assert!(screen[2].contains("Nothing matches"), "{}", screen[2]);
    }
}
