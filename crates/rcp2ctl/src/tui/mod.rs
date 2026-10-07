//! Interactive terminal interface: `rcp2ctl` without a subcommand.

mod app;
mod board;
mod view;

use std::path::Path;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event};
use rcp2_audio::{Graph, PersistPaths, PwError, persist_state, snapshot};

use self::app::{Action, App, Level};
use self::board::{BoardIssue, BoardView, Patience};
use self::view::Mode;
use crate::settings::{self, Settings};
use crate::{CliError, Switch, outputs, persist, prepare, route, set_default};

/// How often the graph is re-read when nothing happens.
pub(crate) const REFRESH_EVERY: Duration = Duration::from_secs(1);
/// How often the board service is asked: often enough for a moving fader to
/// look live (its state is in memory, so an answer is cheap).
pub(crate) const BOARD_EVERY: Duration = Duration::from_millis(250);
/// How long to wait for a key before handling background refreshes.
const INPUT_POLL: Duration = Duration::from_millis(100);

/// A graph snapshot and when it was started: older ones are ignored.
type Snapshot = (Instant, Result<Graph, PwError>);

/// Asks the board service for the board state.
fn board_view() -> BoardView {
    crate::daemon::query_state().map_err(|err| BoardIssue::from(&err))
}

/// Asks the board service for the board state every [`BOARD_EVERY`], on its
/// own thread: a slow service never delays the graph or the keyboard.
fn spawn_board_watcher() -> Receiver<BoardView> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        loop {
            if sender.send(board_view()).is_err() {
                break;
            }
            thread::sleep(BOARD_EVERY);
        }
    });
    receiver
}

pub(crate) fn run() -> Result<(), CliError> {
    let settings_path = settings::settings_path()?;
    let mut settings = settings::load(&settings_path)?;
    // Before the alternate screen, so its notice is visible after quitting.
    let mut app = App::new(prepare(&settings)?);
    let boards = spawn_board_watcher();
    let (requests, snapshots) = spawn_refresher();

    let mut terminal = ratatui::try_init()?;
    let result = event_loop(
        &mut terminal,
        &mut app,
        &mut settings,
        &settings_path,
        &requests,
        &snapshots,
        &boards,
    );
    ratatui::restore();
    result
}

/// Re-reads the graph on its own thread, every second or as soon as asked,
/// so `pw-dump` never blocks the keyboard. Stops when the UI side is dropped.
fn spawn_refresher() -> (Sender<()>, Receiver<Snapshot>) {
    let (request_sender, requests) = mpsc::channel::<()>();
    let (snapshot_sender, snapshots) = mpsc::channel();
    thread::spawn(move || {
        while let Ok(()) | Err(RecvTimeoutError::Timeout) = requests.recv_timeout(REFRESH_EVERY) {
            let started = Instant::now();
            if snapshot_sender.send((started, snapshot())).is_err() {
                break;
            }
        }
    });
    (request_sender, snapshots)
}

/// Reads how the outputs are kept; an unreadable state is reported, not guessed.
fn current_mode(settings: &Settings) -> (Mode, Option<String>) {
    let persist = PersistPaths::from_env().and_then(|paths| persist_state(&paths));
    let error = persist.as_ref().err().map(ToString::to_string);
    let mode = Mode {
        outputs_on: settings.outputs,
        persist: persist.ok(),
    };
    (mode, error)
}

fn event_loop(
    terminal: &mut DefaultTerminal,
    app: &mut App,
    settings: &mut Settings,
    settings_path: &Path,
    requests: &Sender<()>,
    snapshots: &Receiver<Snapshot>,
    boards: &Receiver<BoardView>,
) -> Result<(), CliError> {
    let (mut mode, error) = current_mode(settings);
    if let Some(error) = error {
        app.message = Some((Level::Error, error));
    }
    // Snapshots started before this instant predate the last action.
    let mut fresh_after = Instant::now();
    // Whether the message line holds a refresh error, cleared by the next success.
    let mut refresh_error_shown = false;
    let mut watcher_gone = false;
    loop {
        terminal.draw(|frame| view::render(frame, app, mode))?;

        while !watcher_gone {
            match boards.try_recv() {
                Ok(board) => app.console.record(board),
                Err(TryRecvError::Empty) => break,
                // Its thread is gone: say so rather than show a frozen state.
                Err(TryRecvError::Disconnected) => {
                    let issue = BoardIssue {
                        text: "The board service is no longer asked: restart rcp2ctl.".to_owned(),
                        patience: Patience::None,
                    };
                    app.console.record(Err(issue));
                    watcher_gone = true;
                }
            }
        }
        while let Ok((started, refresh)) = snapshots.try_recv() {
            if started < fresh_after {
                continue;
            }
            match refresh {
                Ok(graph) => {
                    app.set_graph(graph);
                    if refresh_error_shown {
                        app.message = None;
                        refresh_error_shown = false;
                    }
                }
                Err(err) => {
                    app.message = Some((Level::Error, err.to_string()));
                    refresh_error_shown = true;
                }
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
                perform(&action, app, settings, settings_path);
                refresh_error_shown = false;
                let (new_mode, error) = current_mode(settings);
                mode = new_mode;
                if let Some(error) = error {
                    app.message = Some((Level::Error, error));
                }
                fresh_after = Instant::now();
                // A send error means the refresher stopped: the next draw shows
                // the last known graph, which is the best that can be done.
                let _ = requests.send(());
            }
        }
    }
}

/// Runs an action through the same code as the CLI, and shows its output (or
/// error) on the message line.
fn perform(action: &Action, app: &mut App, settings: &mut Settings, path: &Path) {
    let mut text = Vec::new();
    let mut notices = Vec::new();
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
            outputs(path, settings, state, &mut text, &mut |notice| {
                notices.push(notice.to_owned());
            })
        }
        Action::EnablePersist => persist(&app.graph, settings, Switch::On, &mut text),
        Action::RestoreOriginal => persist(&app.graph, settings, Switch::Off, &mut text),
        Action::Refresh | Action::None | Action::Quit => Ok(()),
    };
    app.message = match result {
        Ok(()) => {
            let mut lines = notices;
            lines.push(String::from_utf8_lossy(&text).trim().to_owned());
            let text = lines
                .into_iter()
                .filter(|line| !line.is_empty())
                .collect::<Vec<_>>()
                .join("  ")
                .replace('\n', "  ");
            (!text.is_empty()).then_some((Level::Info, text))
        }
        Err(err) => Some((Level::Error, err.to_string())),
    };
}
