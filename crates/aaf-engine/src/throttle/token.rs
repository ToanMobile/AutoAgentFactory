//! The token that proves a process runs below the holder of a heavy slot.
//!
//! A command started inside a slot must not wait for a slot again: it would
//! wait for its own ancestor. The slot number alone cannot be the proof, as
//! anybody can write it into a variable and skip the queue. So every holder
//! draws a random token, writes it into its lock file and hands
//! `<slot>:<token>` to its command in [`SLOT_ENV`]; a process is inside a
//! slot only when the token it carries is the one in the lock file of a slot
//! that is held right now.

use std::fmt;
use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

use super::slots;
use super::watchdog::{GroupControl, SystemGroup};

/// Set on every command the throttle starts, to `<slot>:<token>`: the slot
/// it was started in and the token of that run. An `aaf slot` started below
/// such a command runs its own command at once ([`inherited_slot`]).
pub const SLOT_ENV: &str = "AAF_HEAVY_SLOT";

/// 128 random bits.
const TOKEN_BYTES: usize = 16;
const TOKEN_HEX_LEN: usize = TOKEN_BYTES * 2;

/// 32 lowercase hex digits. Never printed by `{:?}`: a guard ends up in
/// debug output, and the token is what keeps others out of the slot.
#[derive(Clone, PartialEq, Eq)]
pub struct SlotToken(String);

impl fmt::Debug for SlotToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SlotToken(..)")
    }
}

impl SlotToken {
    /// A fresh token from the operating system's random source. The
    /// workspace has no random-number dependency; `/dev/urandom` is what
    /// `aaf_fp::load_or_create_key` reads too.
    pub fn generate() -> io::Result<SlotToken> {
        let mut bytes = [0u8; TOKEN_BYTES];
        File::open("/dev/urandom")?.read_exact(&mut bytes)?;
        Ok(SlotToken(
            bytes.iter().map(|byte| format!("{byte:02x}")).collect(),
        ))
    }

    /// `None` unless `text` is exactly a token: an empty, short, long or
    /// otherwise malformed value is no token at all, so it can never be
    /// compared equal to one.
    pub fn parse(text: &str) -> Option<SlotToken> {
        let well_formed = text.len() == TOKEN_HEX_LEN
            && text
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
        well_formed.then(|| SlotToken(text.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Compares every byte whatever the outcome. Both sides have the same
    /// length by construction.
    pub fn matches(&self, other: &SlotToken) -> bool {
        self.0
            .bytes()
            .zip(other.0.bytes())
            .fold(0u8, |diff, (a, b)| diff | (a ^ b))
            == 0
    }
}

/// The value of [`SLOT_ENV`] for a command started in slot `index`.
pub fn slot_env_value(index: usize, token: &SlotToken) -> String {
    format!("{index}:{}", token.as_str())
}

/// The slot whose holder started an ancestor of this process, given the
/// [`SLOT_ENV`] value this process inherited. `None` (take your turn in the
/// queue) unless the value is `<slot>:<token>` and that token stands in the
/// lock file of a slot that is held now.
///
/// A command paused in a later slot moves to slot 0 and takes its token
/// along, so slot 0 is looked at as well — under the same rule: held, and
/// holding this very token. Slot 0 being merely busy opens nothing.
pub fn inherited_slot(data_dir: &Path, value: &str) -> Option<usize> {
    let (index, token) = value.split_once(':')?;
    if index.is_empty() || !index.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let index = index.parse::<usize>().ok()?;
    let token = SlotToken::parse(token)?;
    [index, 0]
        .into_iter()
        // With a look at the process table: below the command of a killed
        // holder the slot is no longer locked, and still this one's.
        .find(|slot| {
            slots::holds_token_in(data_dir, *slot, &token, &|pgid| {
                SystemGroup.group_state(pgid)
            })
        })
}
