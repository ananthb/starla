//! Pause state shared between the probe and the tray.
//!
//! Stored as a single-line file at `paths::paused_until_path()`:
//! - absent file → not paused
//! - file containing `indefinite` → paused with no end time
//! - file containing an RFC 3339 timestamp → paused until that instant
//!
//! Both the probe (which honours the pause in its scheduler) and the
//! tray (which writes the file when the user picks a duration) read and
//! write through this module so the format stays in sync.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "until")]
pub enum PauseState {
    Indefinite,
    Until(DateTime<Utc>),
}

impl PauseState {
    /// True if the pause is still in effect at `now`.
    pub fn is_active(&self, now: DateTime<Utc>) -> bool {
        match self {
            PauseState::Indefinite => true,
            PauseState::Until(t) => *t > now,
        }
    }
}

/// Read the current pause state from disk. Returns `None` when the file
/// is missing, empty, or the timestamp has already elapsed (treated as
/// "no longer paused"; callers may delete the stale file).
pub fn read_pause_state() -> Option<PauseState> {
    let path = crate::paths::paused_until_path();
    let contents = std::fs::read_to_string(&path).ok()?;
    parse_pause_state(contents.trim())
}

fn parse_pause_state(s: &str) -> Option<PauseState> {
    if s.is_empty() {
        return None;
    }
    if s.eq_ignore_ascii_case("indefinite") {
        return Some(PauseState::Indefinite);
    }
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|t| PauseState::Until(t.with_timezone(&Utc)))
}

/// Write the pause state to disk, or remove the file when `None`.
pub fn write_pause_state(state: Option<PauseState>) -> std::io::Result<()> {
    let path = crate::paths::paused_until_path();
    match state {
        None => match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        },
        Some(PauseState::Indefinite) => {
            crate::paths::ensure_state_dir()?;
            std::fs::write(&path, "indefinite\n")
        }
        Some(PauseState::Until(t)) => {
            crate::paths::ensure_state_dir()?;
            std::fs::write(&path, format!("{}\n", t.to_rfc3339()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_indefinite() {
        assert_eq!(
            parse_pause_state("indefinite"),
            Some(PauseState::Indefinite)
        );
        assert_eq!(
            parse_pause_state("INDEFINITE"),
            Some(PauseState::Indefinite)
        );
    }

    #[test]
    fn parses_rfc3339() {
        let s = "2030-01-01T00:00:00Z";
        match parse_pause_state(s) {
            Some(PauseState::Until(_)) => {}
            other => panic!("expected Until, got {:?}", other),
        }
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(parse_pause_state(""), None);
        assert_eq!(parse_pause_state("nope"), None);
    }

    #[test]
    fn is_active_indefinite() {
        assert!(PauseState::Indefinite.is_active(Utc::now()));
    }

    #[test]
    fn is_active_until() {
        let past = Utc::now() - chrono::Duration::hours(1);
        let future = Utc::now() + chrono::Duration::hours(1);
        assert!(!PauseState::Until(past).is_active(Utc::now()));
        assert!(PauseState::Until(future).is_active(Utc::now()));
    }
}
