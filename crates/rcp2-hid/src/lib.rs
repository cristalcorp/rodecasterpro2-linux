//! `hidraw` transport for the RØDECaster Pro II control interface (Linux).
//!
//! Plain file I/O on `/dev/hidrawN`: no C library, no `unsafe`. The only
//! writes this crate can perform are the two reports of the handshake, built
//! by [`rcp2_proto`], where the dangerous mode bytes are not representable.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use rcp2_proto::{KNOWN_PRODUCT_IDS, ModeCommand, VENDOR_ID, session_open_report};

/// Where the kernel lists `hidraw` devices.
pub const SYS_CLASS_HIDRAW: &str = "/sys/class/hidraw";

/// The udev rule giving the logged-in user access to the control interface,
/// embedded so that `rcp2ctl hid setup` can install it without any file.
pub const UDEV_RULE: &str = include_str!("../../../packaging/udev/70-rodecaster-pro-2.rules");

/// Where `rcp2ctl hid setup` installs [`UDEV_RULE`].
pub const UDEV_RULE_PATH: &str = "/etc/udev/rules.d/70-rodecaster-pro-2.rules";

/// First line of every rule this project writes: how a file is recognised as ours.
const UDEV_RULE_MARKER: &str = "# rodecasterpro2-linux:";

/// State of the udev rule file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleState {
    /// No file at the rule path.
    Absent,
    /// Our rule, identical to [`UDEV_RULE`].
    UpToDate,
    /// Our rule, from another version.
    Outdated,
    /// A file we did not write: never touched.
    Foreign,
}

/// Classifies the contents of the rule path (`None`: no file).
#[must_use]
pub fn rule_state(contents: Option<&str>) -> RuleState {
    match contents {
        None => RuleState::Absent,
        Some(text) if text == UDEV_RULE => RuleState::UpToDate,
        Some(text) if text.starts_with(UDEV_RULE_MARKER) => RuleState::Outdated,
        Some(_) => RuleState::Foreign,
    }
}

/// Pause between the two handshake reports, as observed with RØDE's app.
const HANDSHAKE_PAUSE: Duration = Duration::from_millis(200);

/// Largest report the device sends: report ID plus 255 data bytes.
const MAX_REPORT_LEN: usize = 256;

/// Error while finding or talking to the board.
#[derive(Debug, thiserror::Error)]
pub enum HidError {
    /// No board found among the `hidraw` devices.
    #[error("no RØDECaster Pro II control interface found (is it plugged in?)")]
    NotFound,
    /// Several boards: which one to use is ambiguous.
    #[error("{0} RØDECaster Pro II control interfaces found; only one is supported")]
    Multiple(usize),
    /// Opening, reading or writing a file failed.
    #[error("{path}: {source}{hint}", hint = permission_hint(source))]
    Io {
        /// File involved.
        path: PathBuf,
        /// Underlying error.
        source: io::Error,
    },
    /// The kernel accepted only part of a report.
    #[error("{path}: short write ({written} of {expected} bytes)")]
    ShortWrite {
        /// Device written to.
        path: PathBuf,
        /// Bytes accepted.
        written: usize,
        /// Bytes in the report.
        expected: usize,
    },
}

fn permission_hint(source: &io::Error) -> &'static str {
    if source.kind() == io::ErrorKind::PermissionDenied {
        " (run `rcp2ctl hid setup` to give your user access)"
    } else {
        ""
    }
}

fn io_err(path: &Path) -> impl FnOnce(io::Error) -> HidError + use<> {
    let path = path.to_owned();
    move |source| HidError::Io { path, source }
}

/// Vendor and product IDs from a `uevent` file's `HID_ID=bus:vendor:product`.
fn parse_hid_id(uevent: &str) -> Option<(u16, u16)> {
    let id = uevent
        .lines()
        .find_map(|line| line.strip_prefix("HID_ID="))?;
    let mut fields = id.split(':');
    let _bus = fields.next()?;
    let vendor = u32::from_str_radix(fields.next()?, 16).ok()?;
    let product = u32::from_str_radix(fields.next()?, 16).ok()?;
    Some((u16::try_from(vendor).ok()?, u16::try_from(product).ok()?))
}

/// The `/dev/hidrawN` nodes of known boards, listed from `sys_class`
/// (normally [`SYS_CLASS_HIDRAW`]), sorted.
///
/// # Errors
///
/// Returns [`HidError::Io`] if `sys_class` cannot be listed.
pub fn find_devices(sys_class: &Path) -> Result<Vec<PathBuf>, HidError> {
    let mut found = Vec::new();
    for entry in fs::read_dir(sys_class).map_err(io_err(sys_class))? {
        let entry = entry.map_err(io_err(sys_class))?;
        // Unreadable entries are simply not ours.
        let Ok(uevent) = fs::read_to_string(entry.path().join("device").join("uevent")) else {
            continue;
        };
        if let Some((vendor, product)) = parse_hid_id(&uevent)
            && vendor == VENDOR_ID
            && KNOWN_PRODUCT_IDS.contains(&product)
        {
            found.push(Path::new("/dev").join(entry.file_name()));
        }
    }
    found.sort();
    Ok(found)
}

/// The single board's `/dev/hidrawN` node.
///
/// # Errors
///
/// Returns [`HidError::NotFound`] or [`HidError::Multiple`] unless exactly one
/// board is found, and [`HidError::Io`] if `sys_class` cannot be listed.
pub fn find_device(sys_class: &Path) -> Result<PathBuf, HidError> {
    let mut found = find_devices(sys_class)?;
    match found.len() {
        0 => Err(HidError::NotFound),
        1 => found.pop().ok_or(HidError::NotFound),
        n => Err(HidError::Multiple(n)),
    }
}

/// One input report, with the time it arrived.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// Time since the capture started.
    pub at: Duration,
    /// The report as read from `hidraw`: report ID first.
    pub bytes: Vec<u8>,
}

/// An open control interface.
pub struct Board {
    file: File,
    path: PathBuf,
}

impl Board {
    /// Opens the board's `hidraw` node for reading and writing.
    ///
    /// # Errors
    ///
    /// Returns [`HidError::Io`] if the node cannot be opened (typically a
    /// permission problem: see the udev rule in `packaging/udev`).
    pub fn open(path: &Path) -> Result<Self, HidError> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(io_err(path))?;
        Ok(Self {
            file,
            path: path.to_owned(),
        })
    }

    fn write_report(&mut self, report: &[u8]) -> Result<(), HidError> {
        // One write() is one report for hidraw: a partial write is an error.
        let written = self.file.write(report).map_err(io_err(&self.path))?;
        if written == report.len() {
            Ok(())
        } else {
            Err(HidError::ShortWrite {
                path: self.path.clone(),
                written,
                expected: report.len(),
            })
        }
    }

    /// Sends the handshake: normal mode on report 1, then session-open on
    /// report 3, which subscribes to notifications and starts the state dump.
    /// These are the only writes this crate can perform.
    ///
    /// Start reading ([`Board::reports`]) **before** calling this: the dump
    /// begins at once and reports are lost if nobody reads them.
    ///
    /// # Errors
    ///
    /// Returns [`HidError`] if a report cannot be written.
    pub fn handshake(&mut self) -> Result<(), HidError> {
        self.write_report(&ModeCommand::Normal.report())?;
        thread::sleep(HANDSHAKE_PAUSE);
        self.write_report(&session_open_report())
    }

    /// Starts a thread reading every input report into the returned channel,
    /// timestamped from `start`. The thread stops at the first read error or
    /// when the receiver is dropped and the next report arrives.
    ///
    /// # Errors
    ///
    /// Returns [`HidError::Io`] if the node cannot be duplicated for reading.
    pub fn reports(&self, start: Instant) -> Result<Receiver<io::Result<Report>>, HidError> {
        let mut file = self.file.try_clone().map_err(io_err(&self.path))?;
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let mut buffer = [0; MAX_REPORT_LEN];
            loop {
                let report = file.read(&mut buffer).and_then(|len| {
                    buffer
                        .get(..len)
                        .map(|bytes| Report {
                            at: start.elapsed(),
                            bytes: bytes.to_vec(),
                        })
                        .ok_or_else(|| io::Error::other("read past the buffer"))
                });
                let failed = report.is_err();
                if sender.send(report).is_err() || failed {
                    break;
                }
            }
        });
        Ok(receiver)
    }
}

/// What [`capture`] recorded.
#[derive(Debug)]
pub struct Capture {
    /// Every report received, in order.
    pub reports: Vec<Report>,
    /// The read error that ended the capture early, if any: the reports
    /// received before it are kept.
    pub error: Option<HidError>,
}

/// Handshakes and records every input report until `idle` passes without a
/// report (after the first one) or `limit` is reached.
///
/// The reading thread stays blocked on the device after this returns; that is
/// fine for a one-shot command, and a long-running owner of the session keeps
/// a single reader instead.
///
/// # Errors
///
/// Returns [`HidError`] if reading cannot start or the handshake fails (the
/// board then received nothing, or only the mode report). A read error during
/// the capture is reported in [`Capture::error`] instead, with the reports
/// received so far.
pub fn capture(board: &mut Board, limit: Duration, idle: Duration) -> Result<Capture, HidError> {
    let start = Instant::now();
    let reports = board.reports(start)?;
    board.handshake()?;
    let mut captured = Vec::new();
    let mut error = None;
    loop {
        let left = limit.saturating_sub(start.elapsed());
        if left.is_zero() {
            break;
        }
        let wait = if captured.is_empty() {
            left
        } else {
            idle.min(left)
        };
        match reports.recv_timeout(wait) {
            Ok(Ok(report)) => captured.push(report),
            Ok(Err(err)) => {
                error = Some(io_err(&board.path)(err));
                break;
            }
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => break,
        }
    }
    Ok(Capture {
        reports: captured,
        error,
    })
}

#[cfg(test)]
mod tests {
    use super::{find_device, find_devices, parse_hid_id};
    use std::fs;
    use std::path::{Path, PathBuf};

    #[test]
    fn recognises_our_rule_and_leaves_others_alone() {
        use super::{RuleState, UDEV_RULE, rule_state};
        assert!(UDEV_RULE.starts_with(super::UDEV_RULE_MARKER));
        assert!(UDEV_RULE.contains("TAG+=\"uaccess\""));
        assert_eq!(rule_state(None), RuleState::Absent);
        assert_eq!(rule_state(Some(UDEV_RULE)), RuleState::UpToDate);
        assert_eq!(
            rule_state(Some("# rodecasterpro2-linux: older\n")),
            RuleState::Outdated
        );
        assert_eq!(
            rule_state(Some("SUBSYSTEM==\"hidraw\"\n")),
            RuleState::Foreign
        );
    }

    #[test]
    fn parses_the_hid_id_line() {
        let uevent = "DRIVER=hid-generic\nHID_ID=0003:000019F7:00000078\nHID_NAME=RØDE\n";
        assert_eq!(parse_hid_id(uevent), Some((0x19F7, 0x0078)));
        assert_eq!(parse_hid_id("HID_ID=0003:zz:00000078"), None);
        assert_eq!(parse_hid_id("HID_ID=0003:100000000:1"), None);
        assert_eq!(parse_hid_id("HID_NAME=x"), None);
    }

    fn fake_sysfs(name: &str, devices: &[(&str, &str)]) -> PathBuf {
        let root = std::env::temp_dir().join(format!("rcp2-hid-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        for (node, hid_id) in devices {
            let dir = root.join(node).join("device");
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("uevent"), format!("HID_ID={hid_id}\n")).unwrap();
        }
        root
    }

    #[test]
    fn finds_only_known_boards() {
        let root = fake_sysfs(
            "known",
            &[
                ("hidraw0", "0003:0000046D:0000C52B"),
                ("hidraw7", "0003:000019F7:00000078"),
                // Another RØDE product, or an unverified mode: not trusted.
                ("hidraw8", "0003:000019F7:00000037"),
            ],
        );
        assert_eq!(find_devices(&root).unwrap(), [Path::new("/dev/hidraw7")]);
        assert_eq!(find_device(&root).unwrap(), Path::new("/dev/hidraw7"));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn reports_missing_and_ambiguous_boards() {
        let none = fake_sysfs("none", &[("hidraw0", "0003:0000046D:0000C52B")]);
        assert!(matches!(find_device(&none), Err(super::HidError::NotFound)));
        let two = fake_sysfs(
            "two",
            &[
                ("hidraw1", "0003:000019F7:00000078"),
                ("hidraw2", "0003:000019F7:00000078"),
            ],
        );
        assert!(matches!(
            find_device(&two),
            Err(super::HidError::Multiple(2))
        ));
        fs::remove_dir_all(&none).unwrap();
        fs::remove_dir_all(&two).unwrap();
    }
}
