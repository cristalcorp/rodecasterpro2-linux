//! Rendering. Reads the state, never changes it.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Cell, Clear, List, ListItem, ListState, Paragraph, Row, Table, TableState,
};
use rcp2_audio::{Channel, PersistState};

use super::app::{App, Focus, Level};
use crate::target_label;

/// Golden yellow accent.
const ACCENT: Color = Color::Rgb(255, 191, 0);

/// What the header shows about how the outputs are kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Mode {
    pub(crate) outputs_on: bool,
    pub(crate) persist: PersistState,
}

pub(crate) fn render(frame: &mut Frame<'_>, app: &App, mode: Mode) {
    let [header, body, message, keys] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(8),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());
    let [outputs, apps] =
        Layout::horizontal([Constraint::Length(30), Constraint::Min(40)]).areas(body);

    frame.render_widget(header_line(app, mode), header);
    render_outputs(frame, app, outputs);
    render_apps(frame, app, apps);
    frame.render_widget(message_line(app), message);
    frame.render_widget(keys_line(app), keys);
    if app.show_help {
        render_help(frame);
    }
}

fn header_line(app: &App, mode: Mode) -> Paragraph<'static> {
    let board = if app.graph.rode().is_ok() {
        Span::styled(
            "RØDECaster Pro II",
            Style::new().fg(ACCENT).add_modifier(Modifier::BOLD),
        )
    } else {
        Span::styled("RØDECaster Pro II not found", Style::new().fg(Color::Red))
    };
    let outputs = match (mode.persist, mode.outputs_on) {
        (PersistState::On, _) => "outputs: on, kept by config file",
        (_, true) => "outputs: on, created at launch",
        (_, false) => "outputs: off",
    };
    let default = default_channel(app).map_or_else(
        || {
            app.graph
                .default_sink()
                .and_then(|name| app.graph.nodes().iter().find(|node| node.name == name))
                .and_then(|node| node.description.clone())
                .unwrap_or_else(|| "?".to_owned())
        },
        Channel::description,
    );
    Paragraph::new(Line::from(vec![
        board,
        Span::raw("  ·  "),
        Span::raw(outputs),
        Span::raw("  ·  default: "),
        Span::styled(default, Style::new().fg(ACCENT)),
    ]))
}

/// The named output the system's default output plays on: our own sink, or
/// the board's native stereo sink, which is the Chat channel too.
fn default_channel(app: &App) -> Option<Channel> {
    let default = app.graph.default_sink()?;
    Channel::from_sink_name(default).or_else(|| {
        app.graph
            .rode()
            .ok()
            .filter(|rode| rode.stereo_sink == default)
            .map(|_| Channel::Chat)
    })
}

fn panel(title: &str, focused: bool) -> Block<'_> {
    let style = if focused {
        Style::new().fg(ACCENT)
    } else {
        Style::new().fg(Color::DarkGray)
    };
    Block::bordered().title(title).border_style(style)
}

fn render_outputs(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let streams = app.graph.app_streams();
    let default = default_channel(app);
    let items: Vec<ListItem<'_>> = Channel::ALL
        .into_iter()
        .zip(1..)
        .map(|(channel, number)| {
            let present = app.graph.virtual_sink(channel).is_some();
            let playing = streams
                .iter()
                .filter(|stream| {
                    stream
                        .targets
                        .iter()
                        .any(|node| Channel::from_sink_name(&node.name) == Some(channel))
                })
                .count();
            let mut spans = vec![
                Span::styled(format!("{number} "), Style::new().fg(ACCENT)),
                Span::raw(format!("{:<11}", channel.description())),
            ];
            if !present {
                spans.push(Span::styled(" absent", Style::new().fg(Color::Red)));
            } else if playing > 0 {
                spans.push(Span::raw(format!(" ♪{playing}")));
            }
            if default == Some(channel) {
                spans.push(Span::styled(" ★", Style::new().fg(ACCENT)));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();
    let focused = app.focus == Focus::Outputs;
    let list = List::new(items)
        .block(panel(" Outputs ", focused))
        .highlight_style(selection_style(focused));
    let mut state = ListState::default().with_selected(Some(app.output_selected));
    frame.render_stateful_widget(list, area, &mut state);
}

fn render_apps(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let streams = app.graph.app_streams();
    let focused = app.focus == Focus::Apps;
    let block = panel(" Applications ", focused);
    if streams.is_empty() {
        let empty = Paragraph::new("No application is playing audio.").block(block);
        frame.render_widget(empty, area);
        return;
    }
    let rows = streams.iter().map(|stream| {
        let output = if stream.targets.is_empty() {
            "(not connected)".to_owned()
        } else {
            stream
                .targets
                .iter()
                .map(|node| target_label(node))
                .collect::<Vec<_>>()
                .join(", ")
        };
        Row::new(vec![
            Cell::from(stream.node.id.to_string()),
            Cell::from(
                stream
                    .node
                    .process_binary
                    .clone()
                    .unwrap_or_else(|| "-".to_owned()),
            ),
            Cell::from(stream.app_label().to_owned()),
            Cell::from(output),
        ])
    });
    let header = Row::new(["ID", "BINARY", "APPLICATION", "OUTPUT"])
        .style(Style::new().add_modifier(Modifier::BOLD));
    let table = Table::new(
        rows,
        [
            Constraint::Length(6),
            Constraint::Length(14),
            Constraint::Min(16),
            Constraint::Length(22),
        ],
    )
    .header(header)
    .block(block)
    .row_highlight_style(selection_style(focused));
    let mut state = TableState::default().with_selected(Some(app.stream_selected));
    frame.render_stateful_widget(table, area, &mut state);
}

fn selection_style(focused: bool) -> Style {
    if focused {
        Style::new().fg(Color::Black).bg(ACCENT)
    } else {
        Style::new().add_modifier(Modifier::REVERSED)
    }
}

fn message_line(app: &App) -> Paragraph<'_> {
    match &app.message {
        Some((Level::Error, text)) => {
            Paragraph::new(text.as_str()).style(Style::new().fg(Color::Red))
        }
        Some((Level::Info, text)) => Paragraph::new(text.as_str()),
        None => Paragraph::new(""),
    }
}

fn keys_line(app: &App) -> Paragraph<'static> {
    let context = match app.focus {
        Focus::Apps => "1-6 send app to output",
        Focus::Outputs => "Enter make default",
    };
    let keys = format!("↑↓ select  {context}  Tab switch  o outputs  p persist  ? help  q quit");
    Paragraph::new(keys).style(Style::new().fg(Color::DarkGray))
}

const HELP: &[(&str, &str)] = &[
    ("↑ ↓ / j k", "select"),
    ("Tab ← →", "switch panel"),
    ("1 … 6", "send the selected application to that output"),
    ("Enter / d", "make the selected output the default"),
    ("o", "named outputs on / off"),
    ("p", "keep outputs after reboot (config file) on / off"),
    ("r", "refresh now"),
    ("q / Esc", "quit"),
];

fn render_help(frame: &mut Frame<'_>) {
    let area = centered(frame.area(), 70, 13);
    let mut lines: Vec<Line<'_>> = HELP
        .iter()
        .map(|(key, what)| {
            Line::from(vec![
                Span::styled(format!("{key:<12}"), Style::new().fg(ACCENT)),
                Span::raw(*what),
            ])
        })
        .collect();
    lines.push(Line::raw(""));
    lines.push(Line::raw(
        "★ default output   ♪ applications playing   any key closes",
    ));
    frame.render_widget(Clear, area);
    frame.render_widget(Paragraph::new(lines).block(panel(" Help ", true)), area);
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let [area] = Layout::horizontal([Constraint::Length(width)])
        .flex(ratatui::layout::Flex::Center)
        .areas(area);
    let [area] = Layout::vertical([Constraint::Length(height)])
        .flex(ratatui::layout::Flex::Center)
        .areas(area);
    area
}

#[cfg(test)]
mod tests {
    use super::{Mode, render};
    use crate::tui::app::App;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use rcp2_audio::{Graph, PersistState};

    const DUMP: &str = include_str!("../../../rcp2-audio/tests/fixtures/pw-dump.json");

    fn screen(app: &App) -> String {
        let mut terminal = Terminal::new(TestBackend::new(100, 16)).unwrap();
        let mode = Mode {
            outputs_on: true,
            persist: PersistState::Off,
        };
        terminal.draw(|frame| render(frame, app, mode)).unwrap();
        let buffer = terminal.backend().buffer();
        buffer
            .content()
            .chunks(usize::from(buffer.area.width))
            .map(|row| {
                row.iter()
                    .map(ratatui::buffer::Cell::symbol)
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn shows_outputs_apps_and_the_default() {
        let app = App::new(Graph::from_pw_dump(DUMP).unwrap());
        let screen = screen(&app);
        assert!(screen.contains("default: RØDE Game"), "{screen}");
        assert!(screen.contains("RØDECaster Pro II"), "{screen}");
        assert!(
            screen.contains("outputs: on, created at launch"),
            "{screen}"
        );
        // Game exists in the fixture, is the default, and spotify plays on it.
        assert!(screen.contains("3 RØDE Game   ♪1 ★"), "{screen}");
        assert!(screen.contains("4 RØDE Music  absent"), "{screen}");
        assert!(screen.contains("qbz"), "{screen}");
        assert!(screen.contains("PipeWire ALSA [qbz]"), "{screen}");
    }

    #[test]
    fn help_overlays_the_screen() {
        let mut app = App::new(Graph::from_pw_dump(DUMP).unwrap());
        app.show_help = true;
        assert!(screen(&app).contains("keep outputs after reboot (config file) on / off"));
    }
}
