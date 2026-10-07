//! What the Console panel shows: decided from the board service's answers,
//! pure, so it is tested without a service or a terminal.

use std::time::Duration;

use rcp2_audio::Channel;
use rcp2_proto::InputSource;

use crate::daemon::{DaemonError, StateDto};
use crate::service::RESTART_AFTER;
use crate::{BOARD_BEING_READ, BOARD_NOT_CONNECTED};

/// How long a restarted service takes to listen again after systemd starts
/// it (its first look at the board is then told by `starting`).
const LAUNCH_MARGIN: Duration = Duration::from_secs(1);
/// Polls needed to cover `wait`, at one poll every `BOARD_EVERY` at least.
const fn polls_in(wait: Duration) -> u128 {
    wait.as_millis().div_ceil(super::BOARD_EVERY.as_millis())
}

/// Failed polls in a row through which the last answer is still shown, by
/// kind of failure (see `05-Etats-et-flux`, table T). Counted rather than
/// timed, so late polls cannot shorten the wait; given as durations, so they
/// follow the polling pace.
/// Hung up or absent: a crashed service is restarted (the poll that saw it
/// go, then until it listens again).
const RESTART_POLLS: u128 = 1 + polls_in(RESTART_AFTER.saturating_add(LAUNCH_MARGIN));
/// Busy: a few seconds of load.
const BUSY_POLLS: u128 = polls_in(Duration::from_secs(6));
/// No answer in time: each such poll lasts up to `QUERY_TIMEOUT`, so a
/// frozen service is said after two, whatever the pace.
const TIMED_OUT_POLLS: u128 = 2;
/// Polls in a row without a read board through which the last one is still
/// shown, whatever the answers (T8): a service that keeps crashing, or keeps
/// starting, is said in the end.
const UNREAD_POLLS: u128 = polls_in(Duration::from_secs(20));

const NO_CHANNEL: &str = "No channel is assigned on the board.";
const ASKING: &str = "Asking the board service…";

/// Whether a failure may be waited out, keeping the last state for a moment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Patience {
    /// Said at once.
    None,
    /// The service is there but serving enough clients.
    Busy,
    /// Hung up or absent: most often a restart.
    Gone,
    /// No answer in time.
    TimedOut,
}

/// Why a poll failed, as shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BoardIssue {
    pub(crate) text: String,
    pub(crate) patience: Patience,
}

impl From<&DaemonError> for BoardIssue {
    fn from(err: &DaemonError) -> Self {
        let patience = match err {
            DaemonError::Busy => Patience::Busy,
            DaemonError::Dropped | DaemonError::NotRunning => Patience::Gone,
            DaemonError::NoAnswer => Patience::TimedOut,
            _ => Patience::None,
        };
        Self {
            text: sentence(&err.to_string()),
            patience,
        }
    }
}

/// An error message as a sentence: capital first letter, final period.
fn sentence(text: &str) -> String {
    let mut chars = text.chars();
    let mut out: String = chars
        .next()
        .map(|first| first.to_uppercase().chain(chars).collect())
        .unwrap_or_default();
    if !out.ends_with(['.', ')', '…']) {
        out.push('.');
    }
    out
}

/// What a poll brought: the board state, or why there is none.
pub(crate) type BoardView = Result<StateDto, BoardIssue>;

/// One channel strip as shown (strip `n` sits on fader `n + 1`, verified on
/// hardware; strips past the last fader have none).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Strip {
    pub(crate) fader: Option<usize>,
    pub(crate) source: String,
    pub(crate) code: Option<i32>,
    pub(crate) muted: Option<bool>,
    pub(crate) level: Option<i32>,
}

/// What the Console panel shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BoardShown {
    /// Strips worth showing (empty ones hidden, unreadable ones kept), and
    /// whether they may no longer be current.
    Strips {
        strips: Vec<Strip>,
        refreshing: bool,
    },
    Message(String),
}

impl BoardShown {
    /// The faders carrying a named output's channel, matched by source code.
    pub(crate) fn faders_of(&self, channel: Channel) -> Vec<usize> {
        let (Self::Strips { strips, .. }, Some(code)) =
            (self, channel_source(channel).and_then(InputSource::code))
        else {
            return Vec::new();
        };
        strips
            .iter()
            .filter(|strip| strip.code == Some(code))
            .filter_map(|strip| strip.fader)
            .collect()
    }
}

/// The board's source feeding a named output, when known.
const fn channel_source(channel: Channel) -> Option<InputSource> {
    match channel {
        Channel::Chat => Some(InputSource::Chat),
        Channel::Usb1 => Some(InputSource::Usb1),
        Channel::Game => Some(InputSource::Game),
        Channel::Music => Some(InputSource::Music),
        Channel::A | Channel::B => None,
    }
}

/// The board service's answers over time, and what they mean for the panel.
#[derive(Debug)]
pub(crate) struct Console {
    /// The last answer, while it still stands for the board: forgotten once
    /// a failure is said, so a later passing one cannot bring it back.
    last: Option<StateDto>,
    /// Polls failed in a row since the last answer, by kind of failure.
    busy: u128,
    gone: u128,
    timed_out: u128,
    /// Polls in a row since the last answer with a read board.
    unread: u128,
    shown: BoardShown,
}

impl Console {
    pub(crate) fn new() -> Self {
        Self {
            last: None,
            busy: 0,
            gone: 0,
            timed_out: 0,
            unread: 0,
            shown: BoardShown::Message(ASKING.to_owned()),
        }
    }

    pub(crate) const fn shown(&self) -> &BoardShown {
        &self.shown
    }

    /// Records what a poll brought, and decides the panel (table T of
    /// `05-Etats-et-flux`): a passing failure keeps the last answer for a
    /// few polls, marked refreshing, and so does a service that says it is
    /// starting, up to a bound on polls without a read board; anything else
    /// is said at once.
    pub(crate) fn record(&mut self, view: BoardView) {
        self.unread = self.unread.saturating_add(1);
        // T8.
        let unread_passing = self.unread <= UNREAD_POLLS;
        let issue = match view {
            Ok(state) => {
                self.busy = 0;
                self.gone = 0;
                self.timed_out = 0;
                if read(&state) {
                    self.unread = 0;
                }
                // T5: a restarted service still taking its first look at the
                // board; it bounds that time itself, T8 a service that keeps
                // restarting.
                if state.starting
                    && unread_passing
                    && !read(&state)
                    && let Some(last) = self.last.as_ref().filter(|last| read(last))
                {
                    self.shown = from_state(last, true);
                    return;
                }
                // T1, T6.
                self.shown = from_state(&state, false);
                self.last = Some(state);
                return;
            }
            Err(issue) => issue,
        };
        // T9: each kind of failure against its own limit.
        let counted = match issue.patience {
            Patience::None => None,
            Patience::Busy => Some((&mut self.busy, BUSY_POLLS)),
            Patience::Gone => Some((&mut self.gone, RESTART_POLLS)),
            Patience::TimedOut => Some((&mut self.timed_out, TIMED_OUT_POLLS)),
        };
        let within = counted.is_some_and(|(count, limit)| {
            *count = count.saturating_add(1);
            *count <= limit
        });
        let passing = within && unread_passing;
        self.shown = match &self.last {
            // T2, T3, T4.
            Some(state) if passing => from_state(state, true),
            // T7, T8, and any failure past its limit.
            _ => {
                self.last = None;
                BoardShown::Message(issue.text)
            }
        };
    }
}

/// The board's state is there to show.
const fn read(state: &StateDto) -> bool {
    state.connected && state.state_known
}

/// The panel for an answer; `unanswered` when later polls failed.
fn from_state(state: &StateDto, unanswered: bool) -> BoardShown {
    if !state.connected {
        return BoardShown::Message(BOARD_NOT_CONNECTED.to_owned());
    }
    if !state.state_known {
        return BoardShown::Message(BOARD_BEING_READ.to_owned());
    }
    let empty = InputSource::Empty.code();
    let strips: Vec<Strip> = state
        .channels
        .iter()
        .enumerate()
        .filter(|(_, channel)| channel.code != empty)
        .map(|(position, channel)| Strip {
            fader: (position < state.fader_count).then_some(position + 1),
            source: channel.source.clone(),
            code: channel.code,
            muted: channel.muted,
            level: channel.level,
        })
        .collect();
    if strips.is_empty() {
        return BoardShown::Message(NO_CHANNEL.to_owned());
    }
    BoardShown::Strips {
        strips,
        refreshing: state.refreshing || unanswered,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::{
        BUSY_POLLS, BoardIssue, BoardShown, Console, Patience, RESTART_POLLS, TIMED_OUT_POLLS,
        UNREAD_POLLS,
    };
    use crate::daemon::{ChannelDto, DaemonError, PROTOCOL_VERSION, StateDto};
    use rcp2_audio::Channel;

    /// A board with every kind of strip: muted, current, empty, unreadable,
    /// past the last fader.
    pub(crate) fn board_state(known: bool) -> StateDto {
        let strip = |index, source: &str, code, muted, level| ChannelDto {
            index,
            source: source.to_owned(),
            code,
            muted: Some(muted),
            level,
        };
        StateDto {
            version: PROTOCOL_VERSION,
            connected: true,
            state_known: known,
            refreshing: false,
            starting: false,
            firmware: Some("1.7.6".to_owned()),
            channels: if known {
                vec![
                    strip(0x1A, "Mic 1", Some(0), true, Some(45)),
                    strip(0x1B, "USB 1", Some(7), false, Some(26)),
                    strip(0x1C, "Chat", Some(8), false, Some(28)),
                    strip(0x1D, "Music", Some(13), false, Some(22)),
                    strip(0x1E, "Game", Some(12), false, Some(127)),
                    strip(0x1F, "(empty)", Some(-1), false, None),
                    strip(0x20, "?", None, false, None),
                    strip(0x103, "source 9", Some(9), false, Some(0)),
                ]
            } else {
                vec![]
            },
            fader_count: if known { 7 } else { 0 },
            notifications: 3,
        }
    }

    fn issue(err: &DaemonError) -> BoardIssue {
        BoardIssue::from(err)
    }

    fn failed(err: &DaemonError) -> super::BoardView {
        Err(issue(err))
    }

    fn refreshing(console: &Console) -> Option<bool> {
        match console.shown() {
            BoardShown::Strips { refreshing, .. } => Some(*refreshing),
            BoardShown::Message(_) => None,
        }
    }

    fn text(console: &Console) -> &str {
        match console.shown() {
            BoardShown::Message(text) => text,
            BoardShown::Strips { .. } => "",
        }
    }

    #[test]
    fn errors_read_as_sentences_and_say_whether_to_wait() {
        let older = issue(&DaemonError::ServiceOlder);
        assert!(
            older
                .text
                .starts_with("The board service is older than this rcp2ctl")
        );
        assert!(older.text.contains("`rcp2ctl hid setup`"));
        let not_running = issue(&DaemonError::NotRunning);
        assert!(
            not_running
                .text
                .contains("`rcp2ctl hid setup` installs and starts it")
        );
        assert!(not_running.text.ends_with(')'));
        assert_eq!(older.patience, Patience::None);
        assert_eq!(not_running.patience, Patience::Gone);
        assert_eq!(issue(&DaemonError::Busy).patience, Patience::Busy);
        assert_eq!(issue(&DaemonError::NoAnswer).patience, Patience::TimedOut);
        assert_eq!(issue(&DaemonError::Dropped).patience, Patience::Gone);
        assert_eq!(
            issue(&DaemonError::BadAnswer("x".to_owned())).text,
            "The board service sent an invalid answer: x."
        );
    }

    /// Records the same failure `times` times; whether the strips were kept
    /// each time.
    fn kept_through(console: &mut Console, err: &DaemonError, times: u128) -> bool {
        (0..times).all(|_| {
            console.record(failed(err));
            refreshing(console) == Some(true)
        })
    }

    fn read_console() -> Console {
        let mut console = Console::new();
        console.record(Ok(board_state(true)));
        assert_eq!(refreshing(&console), Some(false));
        console
    }

    fn unplugged() -> StateDto {
        let mut state = board_state(false);
        state.connected = false;
        state
    }

    fn starting(connected: bool) -> StateDto {
        let mut state = board_state(false);
        state.connected = connected;
        state.starting = true;
        state
    }

    #[test]
    fn t2_busy_keeps_the_last_state_up_to_its_limit() {
        let mut console = read_console();
        assert!(kept_through(&mut console, &DaemonError::Busy, BUSY_POLLS));
        console.record(failed(&DaemonError::Busy));
        assert_eq!(text(&console), "The board service is busy.");
    }

    #[test]
    fn t3_a_frozen_service_is_said_after_two_timeouts() {
        let mut console = read_console();
        assert!(kept_through(
            &mut console,
            &DaemonError::NoAnswer,
            TIMED_OUT_POLLS
        ));
        console.record(failed(&DaemonError::NoAnswer));
        assert_eq!(text(&console), "The board service did not answer in time.");
        // Timeouts among other failures count too.
        let mut console = read_console();
        console.record(failed(&DaemonError::NoAnswer));
        console.record(failed(&DaemonError::Busy));
        console.record(failed(&DaemonError::NoAnswer));
        console.record(failed(&DaemonError::NoAnswer));
        assert_eq!(text(&console), "The board service did not answer in time.");
    }

    #[test]
    fn t4_a_service_gone_is_waited_out_for_a_restart() {
        // Crashed during a poll (hung up) or between two (absent at once).
        for first in [DaemonError::Dropped, DaemonError::NotRunning] {
            let mut console = read_console();
            assert!(kept_through(&mut console, &first, 1), "{first}");
            assert!(kept_through(
                &mut console,
                &DaemonError::NotRunning,
                RESTART_POLLS - 1
            ));
            console.record(failed(&DaemonError::NotRunning));
            assert!(text(&console).starts_with("The board service is not running"));
        }
    }

    /// T2, T4, T8: the limits last as long as the table says, whatever the
    /// polling pace.
    #[test]
    fn the_limits_are_durations() {
        let lasts = |polls: u128| super::super::BOARD_EVERY.as_millis() * polls;
        assert!(lasts(BUSY_POLLS) >= 6_000);
        assert!(lasts(UNREAD_POLLS) >= 20_000);
        // The poll that saw it go, then systemd's delay and the launch.
        let restart = crate::service::RESTART_AFTER.as_millis() + 1_000;
        assert!(lasts(RESTART_POLLS - 1) >= restart);
        // Not much longer either: one poll's rounding at most.
        assert!(lasts(BUSY_POLLS - 1) < 6_000);
        assert!(lasts(UNREAD_POLLS - 1) < 20_000);
        assert!(lasts(RESTART_POLLS - 2) < restart);
    }

    #[test]
    fn t5_a_restarted_service_starting_keeps_the_last_state() {
        let mut console = read_console();
        console.record(failed(&DaemonError::NotRunning));
        // Searching the board, then reading it: as long as it says so, within
        // T8's bound.
        for _ in 0..(UNREAD_POLLS - 1) / 2 {
            console.record(Ok(starting(false)));
            console.record(Ok(starting(true)));
        }
        assert_eq!(refreshing(&console), Some(true));
        console.record(Ok(board_state(true)));
        assert_eq!(refreshing(&console), Some(false));
        // Restarted between two polls (`hid setup`): kept too.
        console.record(Ok(starting(true)));
        assert_eq!(refreshing(&console), Some(true));
        // Starting with nothing read to keep: said.
        let mut console = Console::new();
        console.record(Ok(starting(true)));
        assert_eq!(text(&console), crate::BOARD_BEING_READ);
        // The last answer was not a read board either: the new one is said.
        console.record(Ok(starting(false)));
        assert_eq!(text(&console), crate::BOARD_NOT_CONNECTED);
    }

    #[test]
    fn t6_an_answer_without_the_board_is_said_even_while_waiting() {
        for err in [
            DaemonError::Busy,
            DaemonError::NoAnswer,
            DaemonError::Dropped,
            DaemonError::NotRunning,
        ] {
            let mut console = read_console();
            console.record(failed(&err));
            console.record(Ok(unplugged()));
            assert_eq!(text(&console), crate::BOARD_NOT_CONNECTED, "{err}");
            // A restarted service done starting, the board not read.
            let mut console = read_console();
            console.record(failed(&err));
            console.record(Ok(board_state(false)));
            assert_eq!(text(&console), crate::BOARD_BEING_READ, "{err}");
        }
        // Outside any wait too.
        let mut console = read_console();
        console.record(Ok(unplugged()));
        assert_eq!(text(&console), crate::BOARD_NOT_CONNECTED);
    }

    #[test]
    fn t8_a_service_that_keeps_crashing_is_said_in_the_end() {
        // Crashes, restarts, starts, crashes again: each answer alone would
        // keep the last state.
        let crash_loop = || {
            [
                failed(&DaemonError::Dropped),
                failed(&DaemonError::NotRunning),
                Ok(starting(true)),
                Ok(starting(true)),
            ]
            .into_iter()
            .cycle()
        };
        let mut console = read_console();
        let mut polls = crash_loop();
        for poll in polls.by_ref().take(usize::try_from(UNREAD_POLLS).unwrap()) {
            console.record(poll);
            assert_eq!(refreshing(&console), Some(true));
        }
        console.record(polls.next().unwrap());
        assert_eq!(refreshing(&console), None);
        // Restarted between every two polls: only ever starting.
        let mut console = read_console();
        for _ in 0..UNREAD_POLLS {
            console.record(Ok(starting(true)));
            assert_eq!(refreshing(&console), Some(true));
        }
        console.record(Ok(starting(true)));
        assert_eq!(text(&console), crate::BOARD_BEING_READ);
        // Only a read board starts the count again.
        let mut console = read_console();
        for poll in crash_loop().take(usize::try_from(UNREAD_POLLS).unwrap() - 1) {
            console.record(poll);
        }
        console.record(Ok(board_state(true)));
        for poll in crash_loop().take(usize::try_from(UNREAD_POLLS).unwrap()) {
            console.record(poll);
            assert_eq!(refreshing(&console), Some(true));
        }
    }

    #[test]
    fn t9_each_kind_of_failure_has_its_own_limit() {
        // Busy up to its limit, then gone: the restart is still waited out.
        let mut console = read_console();
        assert!(kept_through(&mut console, &DaemonError::Busy, BUSY_POLLS));
        assert!(kept_through(
            &mut console,
            &DaemonError::NotRunning,
            RESTART_POLLS
        ));
        console.record(failed(&DaemonError::NotRunning));
        assert!(text(&console).starts_with("The board service is not running"));
        // An answer starts every count again.
        let mut console = read_console();
        assert!(kept_through(&mut console, &DaemonError::Busy, BUSY_POLLS));
        console.record(Ok(starting(true)));
        assert!(kept_through(&mut console, &DaemonError::Busy, BUSY_POLLS));
    }

    #[test]
    fn a_said_error_is_not_undone_by_a_passing_one() {
        let mut console = Console::new();
        console.record(Ok(board_state(true)));
        console.record(failed(&DaemonError::ServiceOlder));
        // A passing failure after it: the old strips stay gone.
        console.record(failed(&DaemonError::Busy));
        assert_eq!(text(&console), "The board service is busy.");
    }

    #[test]
    fn a_busy_service_keeps_a_message_too() {
        let mut console = Console::new();
        console.record(Ok(board_state(false)));
        console.record(failed(&DaemonError::Busy));
        assert_eq!(text(&console), crate::BOARD_BEING_READ);
    }

    #[test]
    fn a_lasting_failure_is_said_at_once() {
        let mut console = Console::new();
        console.record(Ok(board_state(true)));
        console.record(failed(&DaemonError::ServiceNewer));
        assert!(text(&console).starts_with("The board service is newer"));
        // A passing one with nothing to keep is said too.
        let mut console = Console::new();
        assert_eq!(text(&console), super::ASKING);
        console.record(failed(&DaemonError::Busy));
        assert_eq!(text(&console), "The board service is busy.");
    }

    #[test]
    fn the_service_refreshing_is_shown() {
        let mut console = Console::new();
        let mut state = board_state(true);
        state.refreshing = true;
        console.record(Ok(state));
        assert_eq!(refreshing(&console), Some(true));
    }

    #[test]
    fn says_why_there_are_no_strips() {
        let mut console = Console::new();
        let mut unplugged = board_state(false);
        unplugged.connected = false;
        console.record(Ok(unplugged));
        assert_eq!(text(&console), crate::BOARD_NOT_CONNECTED);
        let mut bare = board_state(true);
        bare.channels.clear();
        console.record(Ok(bare));
        assert_eq!(text(&console), super::NO_CHANNEL);
    }

    #[test]
    fn an_output_on_several_faders_lists_them_all() {
        let mut console = Console::new();
        let mut state = board_state(true);
        // Game on the strips of faders 2 and 5.
        if let Some(channel) = state.channels.get_mut(1) {
            channel.code = Some(12);
        }
        console.record(Ok(state));
        assert_eq!(console.shown().faders_of(Channel::Game), vec![2, 5]);
        assert!(console.shown().faders_of(Channel::A).is_empty());
    }
}
