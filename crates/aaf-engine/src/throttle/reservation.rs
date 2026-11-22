//! What the holder of a slot says about its command: how much memory it is
//! expected to reach, and which process group runs it.

use std::path::Path;

use super::legacy;
use super::slot_file::record_text;
use super::slots::{Admission, required_free};

/// What the holder of a slot wrote into its lock file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reservation {
    /// Memory the command is expected to reach: 1.25 x its recorded peak.
    /// `None` for a command that runs alone.
    pub reserved_bytes: Option<u64>,
    /// The command's process group, once it has started.
    pub pgid: Option<u32>,
}

impl Reservation {
    pub(super) fn of(admission: Admission) -> Reservation {
        Reservation {
            reserved_bytes: match admission {
                Admission::Measured { peak_rss_bytes } => Some(required_free(peak_rss_bytes)),
                Admission::Unmeasured => None,
            },
            pgid: None,
        }
    }

    pub(super) fn render(self) -> String {
        let mut text = match self.reserved_bytes {
            Some(bytes) => format!("reserved={bytes}\n"),
            None => "reserved=unknown\n".to_string(),
        };
        if let Some(pgid) = self.pgid {
            text.push_str(&format!("pgid={pgid}\n"));
        }
        text
    }

    /// `None` unless the text holds a well-formed `reserved=` line.
    pub(super) fn parse(text: &str) -> Option<Reservation> {
        let value = |key: &str| {
            text.lines()
                .find_map(|line| line.strip_prefix(key)?.strip_prefix('='))
        };
        let reserved_bytes = match value("reserved")? {
            "unknown" => None,
            bytes => Some(bytes.parse::<u64>().ok()?),
        };
        Some(Reservation {
            reserved_bytes,
            // 0 and 1 are never a command's group.
            pgid: value("pgid")
                .and_then(|pgid| pgid.parse::<u32>().ok())
                .filter(|pgid| *pgid > 1),
        })
    }
}

/// The process group a lock file's text names, if any.
pub(super) fn recorded_pgid(text: &str) -> Option<u32> {
    Reservation::parse(text)?.pgid
}

/// The reservation in the record of slot `index`'s holder; `None` when the
/// record is missing, empty or not understood. Says nothing about the slot
/// being taken.
pub fn read_reservation(data_dir: &Path, index: usize) -> Option<Reservation> {
    // A holder from before the throttle had its own directory wrote into
    // its own lock file.
    let text = legacy::held_text(data_dir, index).unwrap_or_else(|| record_text(data_dir, index));
    Reservation::parse(&text)
}
