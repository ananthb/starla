//! What the reference probe keeps between runs (reginit.sh's
//! `con_init_conf.txt`, written by atlasinit): the controller the registration
//! server assigned, until when it holds, and any wait a server asked for.
//!
//! The reference registers only on first boot, when forced, or when one of
//! those timers runs out; a restart reuses the saved controller. Starla
//! registered `NEW` on every start, so three restarts in 30 min (2026-10-12,
//! probe 1018212) were three new registrations and three new controller
//! sessions; RIPE answered every upload with 429 from four minutes later.

use crate::ControllerInfo;
use std::path::Path;

/// atlasinit's process_wait clamps every server-given delay to this range.
pub const MIN_WAIT_SECS: i64 = 100;
pub const MAX_WAIT_SECS: i64 = 28 * 24 * 3600;

/// The wait atlasinit forces when a server answers neither OK nor WAIT
/// (ATLAS_DEFAULT_WAIT), and after a controller's REFUSED.
pub const DEFAULT_WAIT_SECS: i64 = 100;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct RegState {
    /// The assigned controller (CONTROLLER_1_HOST / _PORT) and probe id.
    pub controller: Option<ControllerInfo>,
    /// REREG_TIMER: when the assignment runs out (unix seconds).
    pub rereg_at: Option<i64>,
    /// REG_WAIT_UNTIL: a registration server's WAIT (unix seconds).
    pub wait_until: Option<i64>,
    /// force_reg.sh: a controller REFUSED us; register again, whatever the
    /// timers say. Holds the controller's reason, for the log.
    pub forced: Option<String>,
}

/// What to do before talking to a controller.
#[derive(Debug, Clone, PartialEq)]
pub enum RegStep {
    /// Use the saved assignment; skip the registration server.
    Cached(ControllerInfo),
    /// A server asked us to wait until then (unix seconds).
    WaitUntil(i64),
    /// Ask the registration server, with this REASON_FOR_REGISTRATION.
    Register(&'static str),
}

fn clamp_wait(secs: i64) -> i64 {
    secs.clamp(MIN_WAIT_SECS, MAX_WAIT_SECS)
}

impl RegState {
    /// reginit.sh's decision, in its order: a forced registration first, then
    /// a pending wait, then the assignment's timer. An assignment without a
    /// REREGISTER timer is not reused (reginit then keeps R=NEW).
    pub fn next_step(&self, now: i64) -> RegStep {
        if self.forced.is_some() {
            return RegStep::Register("FORCE_REG");
        }
        if let Some(t) = self.wait_until {
            return if now < t {
                RegStep::WaitUntil(t)
            } else {
                RegStep::Register("WAIT_OVER")
            };
        }
        match (&self.controller, self.rereg_at) {
            (Some(c), Some(t)) if now < t => RegStep::Cached(c.clone()),
            (Some(_), Some(_)) => RegStep::Register("REREG_TIMER_EXPIRED"),
            _ => RegStep::Register("NEW"),
        }
    }

    /// The registration server assigned a controller.
    pub fn registered(&mut self, info: &ControllerInfo, now: i64) {
        self.rereg_at = info.rereg_secs.map(|s| now + clamp_wait(s as i64));
        self.controller = Some(info.clone());
        self.wait_until = None;
        self.forced = None;
    }

    /// A registration server said WAIT (or answered something else).
    pub fn told_to_wait(&mut self, secs: i64, now: i64) {
        self.wait_until = Some(now + clamp_wait(secs));
    }

    /// A controller said REFUSED: drop the assignment, register again.
    pub fn refused(&mut self, reason: &str) {
        self.controller = None;
        self.rereg_at = None;
        self.forced = Some(reason.to_string());
    }

    /// Read the saved state; a missing or unreadable file is no state (a
    /// `NEW` registration), as a missing con_init_conf.txt is for reginit.
    pub fn load(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .map(|text| Self::parse(&text))
            .unwrap_or_default()
    }

    /// Write it atomically (temp file, then rename), owner-only.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let tmp = path.with_extension("new");
        std::fs::write(&tmp, self.render())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        }
        std::fs::rename(&tmp, path)
    }

    fn parse(text: &str) -> Self {
        let (mut host, mut port, mut probe_id) = (None, None, 0u32);
        let mut st = Self::default();
        for line in text.lines() {
            let (k, v) = line.split_once(' ').unwrap_or((line, ""));
            let v = v.trim();
            match k {
                "CONTROLLER_1_HOST" if !v.is_empty() => host = Some(v.to_string()),
                "CONTROLLER_1_PORT" => port = v.parse::<u16>().ok(),
                "PROBE_ID" => probe_id = v.parse().unwrap_or(0),
                "REREG_TIMER" => st.rereg_at = v.parse().ok(),
                "REG_WAIT_UNTIL" => st.wait_until = v.parse().ok(),
                "FORCE_REG" => st.forced = Some(v.to_string()),
                _ => {}
            }
        }
        if let (Some(host), Some(port)) = (host, port) {
            st.controller = Some(ControllerInfo {
                host,
                port,
                probe_id,
                rereg_secs: None,
            });
        }
        st
    }

    fn render(&self) -> String {
        let mut out = String::new();
        if let Some(c) = &self.controller {
            out.push_str(&format!(
                "CONTROLLER_1_HOST {}\nCONTROLLER_1_PORT {}\nPROBE_ID {}\n",
                c.host, c.port, c.probe_id
            ));
        }
        if let Some(t) = self.rereg_at {
            out.push_str(&format!("REREG_TIMER {t}\n"));
        }
        if let Some(t) = self.wait_until {
            out.push_str(&format!("REG_WAIT_UNTIL {t}\n"));
        }
        if let Some(r) = &self.forced {
            out.push_str(&format!("FORCE_REG {r}\n"));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse_init_response;
    use crate::InitResponse;

    const NOW: i64 = 1_791_836_000;

    fn ctr(rereg_secs: Option<u64>) -> ControllerInfo {
        ControllerInfo {
            host: "ctr-dub-sw02.atlas.prod.ripe.net".into(),
            port: 443,
            probe_id: 1018212,
            rereg_secs,
        }
    }

    #[test]
    fn a_restart_reuses_the_assignment_until_its_timer_runs_out() {
        let mut st = RegState::default();
        assert_eq!(st.next_step(NOW), RegStep::Register("NEW"));

        st.registered(&ctr(Some(86_400)), NOW);
        let saved = RegState::parse(&st.render());
        assert_eq!(
            saved.next_step(NOW + 60),
            RegStep::Cached(ControllerInfo {
                rereg_secs: None,
                ..ctr(None)
            })
        );
        assert_eq!(
            saved.next_step(NOW + 86_400),
            RegStep::Register("REREG_TIMER_EXPIRED")
        );
    }

    #[test]
    fn without_a_rereg_timer_the_reference_registers_again() {
        let mut st = RegState::default();
        st.registered(&ctr(None), NOW);
        assert_eq!(st.next_step(NOW + 1), RegStep::Register("NEW"));
    }

    #[test]
    fn a_wait_is_kept_across_restarts_and_clamped() {
        let mut st = RegState::default();
        st.told_to_wait(5, NOW);
        let saved = RegState::parse(&st.render());
        assert_eq!(
            saved.next_step(NOW),
            RegStep::WaitUntil(NOW + MIN_WAIT_SECS)
        );
        assert_eq!(
            saved.next_step(NOW + MIN_WAIT_SECS),
            RegStep::Register("WAIT_OVER")
        );

        st.told_to_wait(i64::MAX / 2, NOW);
        assert_eq!(st.wait_until, Some(NOW + MAX_WAIT_SECS));
    }

    #[test]
    fn a_refusal_forces_a_registration() {
        let mut st = RegState::default();
        st.registered(&ctr(Some(86_400)), NOW);
        st.refused("controller restarting");
        let saved = RegState::parse(&st.render());
        assert_eq!(saved.next_step(NOW + 1), RegStep::Register("FORCE_REG"));
        assert_eq!(saved.controller, None);
    }

    #[test]
    fn save_and_load_round_trip_and_missing_is_new() {
        let dir = std::env::temp_dir().join(format!("starla-reg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("con_init_conf.txt");
        assert_eq!(RegState::load(&path), RegState::default());

        let mut st = RegState::default();
        st.registered(&ctr(Some(3_600)), NOW);
        st.save(&path).unwrap();
        let back = RegState::load(&path);
        assert_eq!(back.rereg_at, Some(NOW + 3_600));
        assert_eq!(back.controller.unwrap().probe_id, 1018212);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_init_replies_carry_rereg_and_refused() {
        let reply = "OK\nPROBE_ID 1018212\nCONTROLLER ctr-dub-sw02.atlas.prod.ripe.net 443 ssh-ed25519 AAAA\nREREGISTER 86400\n";
        match parse_init_response(reply).unwrap() {
            InitResponse::Controller(c) => {
                assert_eq!(c.rereg_secs, Some(86_400));
                assert_eq!(c.probe_id, 1018212);
            }
            other => panic!("{other:?}"),
        }
        match parse_init_response("REFUSED\nprobe moved\n").unwrap() {
            InitResponse::Refused { reason } => assert_eq!(reason, "probe moved"),
            other => panic!("{other:?}"),
        }
    }
}
