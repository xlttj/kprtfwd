//! Drawing. Colour only in the STATE column (and red for errors), no
//! symbols, no decoration.

use std::time::SystemTime;

use kprtfwd_core::clock::local_time;
use kprtfwd_core::{ContextState, ForwardState};
use kprtfwd_proto::Snapshot;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Row, Table, TableState};

use crate::app::{App, Discovery, FORM_LABELS, Mode, Services};

const KEYS: &str =
    "space on/off · / filter · +/- all listed · p projects · o open · y copy · ? keys";
/// Errors older than this are not shown next to a ready forward.
const RECENT_ERROR_SECS: u64 = 60;

pub fn draw(frame: &mut Frame, app: &App) {
    let [header, filter, list, message, keys] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    frame.render_widget(header_line(&app.snapshot, header.width), header);
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
        Mode::Discovery(step) => draw_discovery(frame, step),
        Mode::Events { id, entries } => {
            let mut lines: Vec<Line> = entries
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
            if lines.is_empty() {
                lines.push(Line::styled("no events yet", dim()));
            }
            popup(frame, &format!(" events: {id} "), lines, 100);
        }
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
    let version = format!("kprtfwd {} ", snapshot.daemon_version);
    let used: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    let gap = usize::from(width).saturating_sub(used + version.len());
    spans.push(Span::raw(" ".repeat(gap)));
    spans.push(Span::styled(version, dim()));
    Line::from(spans)
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

fn draw_list(frame: &mut Frame, app: &App, area: Rect) {
    let visible = app.visible();
    app.page.set(usize::from(area.height.saturating_sub(1)));
    if visible.is_empty() {
        let text = if app.snapshot.forwards.is_empty() {
            " No forwards yet. d discovers services, a adds one."
        } else {
            " Nothing matches the filter. esc clears it."
        };
        frame.render_widget(Paragraph::new(Line::styled(text, dim())), area);
        return;
    }

    let rows: Vec<Row> = visible
        .iter()
        .map(|view| {
            let f = &view.forward;
            let context = app.snapshot.context_state(&f.context);
            Row::new(vec![
                Span::styled(view.state.label(), state_style(&view.state)),
                Span::raw(f.context.clone()),
                Span::raw(format!("{}/{}:{}", f.namespace, f.service, f.remote_port)),
                Span::raw(f.local_port.to_string()),
                Span::raw(info(view, &context)),
            ])
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
    let mut state = TableState::default().with_selected(app.selected_index());
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

fn draw_projects(frame: &mut Frame, app: &App, selected: usize, naming: Option<&str>) {
    let mut lines: Vec<Line> = app
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
    if lines.is_empty() {
        lines.push(Line::styled("no projects yet", dim()));
    }
    lines.push(Line::raw(""));
    match naming {
        Some(name) => {
            let listed = app.visible().len();
            lines.push(Line::raw(format!(
                "new project with the {listed} listed forwards: {name}_"
            )));
            lines.push(Line::styled("enter create · esc cancel", dim()));
        }
        None => {
            lines.push(Line::styled(
                "enter on · - off · s switch to · f show members",
                dim(),
            ));
            lines.push(Line::styled(
                "n new from list · u set to list · x delete · esc back",
                dim(),
            ));
        }
    }
    popup(frame, " projects ", lines, 64);
}

fn draw_discovery(frame: &mut Frame, step: &Discovery) {
    match step {
        Discovery::Loading(what) => popup(
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
            let mut lines: Vec<Line> = names
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
            lines.push(Line::raw(""));
            lines.push(Line::styled("enter choose · esc cancel", dim()));
            popup(frame, " discover: context ", lines, 60);
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
        Discovery::Services(services) => draw_services(frame, services),
    }
}

fn draw_services(frame: &mut Frame, s: &Services) {
    let area = centered(frame.area(), 110, frame.area().height.saturating_sub(4));
    frame.render_widget(Clear, area);
    let block = Block::bordered().title(format!(" discover: {} ", s.context));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let [filter, list, keys] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
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
    let mut state =
        TableState::default().with_selected((!visible.is_empty()).then_some(s.selected));
    frame.render_stateful_widget(table, list, &mut state);

    let marked = s.ports.iter().filter(|c| c.marked).count();
    frame.render_widget(
        Line::styled(format!("{marked} marked · space mark · +/- all listed · e local port · / filter · enter add · esc cancel"), dim()),
        keys,
    );
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
    let height = u16::try_from(lines.len())
        .unwrap_or(u16::MAX)
        .saturating_add(2);
    let area = centered(frame.area(), width, height);
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
    use crate::app::tests::snapshot;
    use kprtfwd_proto::ContextView;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

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
        assert!(screen[0].ends_with("kprtfwd 2.0.0-test"), "{}", screen[0]);
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
