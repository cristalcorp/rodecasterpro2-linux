//! Keeps the named outputs in place while the board service runs.
//!
//! The outputs go away with the board's sound card (unplug, USB reset): each
//! one is bound to the card and removes itself when the card disappears,
//! rather than sending its sound to another device. Outputs from the
//! PipeWire config file only come back when PipeWire restarts. This watcher
//! recreates the missing ones (at runtime, as at launch) as soon as the card
//! is back, following the same rule as the other commands: only if the named
//! outputs are on.

use std::io::{BufRead as _, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::Duration;

/// Events are grouped until the graph has been quiet this long: the card's
/// two sinks appear one after the other.
const SETTLE: Duration = Duration::from_millis(500);
/// Pause before watching again after `pactl subscribe` stopped (e.g.
/// PipeWire restarting).
const RESUBSCRIBE_EVERY: Duration = Duration::from_secs(5);

/// Watches PipeWire forever, logging what it recreates.
pub(crate) fn run(log: &mut impl Write) {
    loop {
        // Also covers what happened while nothing was watching.
        restore(log);
        match subscribe() {
            Ok(child) => watch(child, log),
            Err(err) => {
                let _ = writeln!(log, "cannot watch PipeWire (pactl subscribe): {err}");
            }
        }
        thread::sleep(RESUBSCRIBE_EVERY);
    }
}

fn subscribe() -> std::io::Result<Child> {
    Command::new("pactl")
        .arg("subscribe")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
}

/// A new sink in `pactl subscribe` output: the only event that can bring the
/// board's card back.
fn is_new_sink(line: &str) -> bool {
    line.starts_with("Event 'new' on sink #")
}

/// Restores the outputs after each burst of new sinks, until `pactl` stops.
fn watch(mut child: Child, log: &mut impl Write) {
    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return;
    };
    let (events, received) = mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if is_new_sink(&line) && events.send(()).is_err() {
                break;
            }
        }
    });
    'watching: while received.recv().is_ok() {
        loop {
            match received.recv_timeout(SETTLE) {
                Ok(()) => {}
                Err(RecvTimeoutError::Timeout) => break,
                // `run` restores again before watching anew.
                Err(RecvTimeoutError::Disconnected) => break 'watching,
            }
        }
        restore(log);
    }
    let _ = child.wait();
}

/// Creates the missing named outputs if they are on and the board is there.
fn restore(log: &mut impl Write) {
    // Settings read under the lock: an `outputs off` in progress is seen.
    let result = crate::outputs_lock().and_then(|_lock| {
        let settings = crate::load_settings()?;
        crate::create_missing_outputs(&settings)
    });
    match result {
        Ok(restored) => {
            for notice in restored.notices() {
                let _ = writeln!(log, "{notice}");
            }
        }
        Err(err) => {
            let _ = writeln!(log, "cannot restore the named outputs: {err}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::is_new_sink;

    #[test]
    fn reacts_to_new_sinks_only() {
        assert!(is_new_sink("Event 'new' on sink #447"));
        assert!(!is_new_sink("Event 'new' on sink-input #446"));
        assert!(!is_new_sink("Event 'change' on sink #447"));
        assert!(!is_new_sink("Event 'remove' on sink #447"));
        assert!(!is_new_sink("Event 'new' on source #447"));
    }
}
