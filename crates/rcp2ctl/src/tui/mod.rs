//! Interactive terminal interface: `rcp2ctl` without a subcommand.

mod app;
mod view;

use std::path::Path;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::Duration;

use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event};
use rcp2_audio::{Graph, PersistPaths, PersistState, PwError, persist_state, snapshot};

use self::app::{Action, App, Level};
use self::view::Mode;
use crate::settings::{self, Settings};
use crate::{CliError, Switch, outputs, persist, prepare, route, set_default};

/// How often the graph is re-read in the background.
const REFRESH_EVERY: Duration = Duration::from_secs(1);
/// How long to wait for a key before handling background refreshes.
const INPUT_POLL: Duration = Duration::from_millis(100);

pub(crate) fn run() -> Result<(), CliError> {
    let settings_path = settings::settings_path()?;
    let mut settings = settings::load(&settings_path)?;
    // Before the alternate screen, so its notice is visible after quitting.
    let mut app = App::new(prepare(&settings)?);
    let refreshes = spawn_refresher();

    let mut terminal = ratatui::init();
    let result = event_loop(
        &mut terminal,
        &mut app,
        &mut settings,
        &settings_path,
        &refreshes,
    );
    ratatui::restore();
    result
}

/// Re-reads the graph every second on its own thread, so slow `pw-dump` runs
/// never block the keyboard. Stops when the receiver is dropped.
fn spawn_refresher() -> Receiver<Result<Graph, PwError>> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        loop {
            thread::sleep(REFRESH_EVERY);
            if sender.send(snapshot()).is_err() {
                break;
            }
        }
    });
    receiver
}

fn current_mode(settings: &Settings) -> Mode {
    let persist = PersistPaths::from_env()
        .and_then(|paths| persist_state(&paths))
        .unwrap_or(PersistState::Foreign);
    Mode {
        outputs_on: settings.outputs,
        persist,
    }
}

fn event_loop(
    terminal: &mut DefaultTerminal,
    app: &mut App,
    settings: &mut Settings,
    settings_path: &Path,
    refreshes: &Receiver<Result<Graph, PwError>>,
) -> Result<(), CliError> {
    let mut mode = current_mode(settings);
    loop {
        terminal.draw(|frame| view::render(frame, app, mode))?;

        while let Ok(refresh) = refreshes.try_recv() {
            match refresh {
                Ok(graph) => app.set_graph(graph),
                Err(err) => app.message = Some((Level::Error, err.to_string())),
            }
        }
        if !event::poll(INPUT_POLL)? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        match app.on_key(key) {
            Action::Quit => return Ok(()),
            Action::None => {}
            action => {
                perform(&action, app, settings, settings_path, mode);
                mode = current_mode(settings);
            }
        }
    }
}

/// Runs an action through the same code as the CLI, and shows its output (or
/// error) on the message line.
fn perform(action: &Action, app: &mut App, settings: &mut Settings, path: &Path, mode: Mode) {
    let mut text = Vec::new();
    let result = match *action {
        Action::Route { stream_id, channel } => {
            route(&app.graph, &stream_id.to_string(), channel, &mut text)
        }
        Action::SetDefault(channel) => set_default(&app.graph, channel, &mut text),
        Action::ToggleOutputs => {
            let state = if settings.outputs {
                Switch::Off
            } else {
                Switch::On
            };
            outputs(path, settings, state, &mut text)
        }
        Action::TogglePersist => {
            let state = if mode.persist == PersistState::On {
                Switch::Off
            } else {
                Switch::On
            };
            persist(&app.graph, settings, state, &mut text)
        }
        Action::Refresh | Action::None | Action::Quit => Ok(()),
    };
    app.message = match result {
        Ok(()) => {
            let text = String::from_utf8_lossy(&text).trim().replace('\n', "  ");
            (!text.is_empty()).then_some((Level::Info, text))
        }
        Err(err) => Some((Level::Error, err.to_string())),
    };
    match snapshot() {
        Ok(graph) => app.set_graph(graph),
        Err(err) => app.message = Some((Level::Error, err.to_string())),
    }
}
