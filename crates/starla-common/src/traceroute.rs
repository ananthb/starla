//! evtraceroute's options, as the reference probe parses them.
//!
//! Shared by the controller, which parses a CRONLINE/ONEOFF into them, and
//! the measurement, which runs them. Names, types and defaults follow
//! traceroute_init (probe-busybox/eperd/traceroute.c:4203-4466, tag 5130);
//! fields eperd keeps in an `unsigned char` are `u8` here, as its
//! assignments from getopt32's ints truncate the same way.

use serde::{Deserialize, Serialize};

/// `-I` ICMP echo, `-T` TCP SYN, otherwise UDP (`do_udp= !(do_icmp ||
/// do_tcp)`, traceroute.c:4281).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TracerouteProtocol {
    ICMP,
    #[default]
    UDP,
    TCP,
}

/// One evtraceroute measurement's options (everything but the target,
/// `-4`/`-6` and the `-A`/`-O` plumbing).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TracerouteOptions {
    pub protocol: TracerouteProtocol,
    /// `-c`: probes per hop (trtcount)
    pub count: u8,
    /// `-f`: first hop limit
    pub first_hop: u8,
    /// `-g`: after this many hops without an answer, one last-ditch hop
    /// (255) and the result
    pub gap_limit: u8,
    /// `-m`: last hop limit
    pub max_hops: u8,
    /// `-S`: data bytes after the ICMP/UDP/TCP header (maxpacksize); the
    /// result's envelope "size"
    pub size: u16,
    /// `-p`: TCP destination port
    pub dest_port: u16,
    /// `-t`: IP TOS / IPv6 traffic class of the probes (0: left alone)
    pub tos: u32,
    /// `-w`: ms to wait for each probe's reply
    pub timeout_ms: u32,
    /// `-z`: ms to wait for duplicates after a reply before the next probe
    pub dup_timeout_ms: u32,
    /// `-a`: Paris traceroute: the id cycles through `paris_mod` values, one
    /// per run; 0 is classic traceroute (and no "paris_id" in the result)
    pub paris_mod: u8,
    /// `-b`: lowest Paris id
    pub paris_base: u8,
    /// `-H`: IPv6 hop-by-hop options header of this many bytes (PadN)
    pub hbh_opt_size: u16,
    /// `-D`: IPv6 destination options header of this many bytes (PadN)
    pub dest_opt_size: u16,
    /// `-F`: set don't-fragment (path MTU discovery) on the probes
    pub dont_fragment: bool,
    /// `-i`: bind the probes to this interface
    pub interface: Option<String>,
    /// `-B`: bundle id, written into the result as "bundle"
    pub bundle: Option<String>,
}

impl Default for TracerouteOptions {
    /// traceroute_init's defaults (traceroute.c:4237-4251).
    fn default() -> Self {
        Self {
            protocol: TracerouteProtocol::UDP,
            count: 3,
            first_hop: 1,
            gap_limit: 5,
            max_hops: 32,
            size: 40,
            dest_port: 80,
            tos: 0,
            timeout_ms: 1000,
            dup_timeout_ms: 10,
            paris_mod: 16,
            paris_base: 0,
            hbh_opt_size: 0,
            dest_opt_size: 0,
            dont_fragment: false,
            interface: None,
            bundle: None,
        }
    }
}

impl TracerouteOptions {
    /// The largest `-S` traceroute_init accepts (MAX_DATA_SIZE,
    /// traceroute.c:46 and 4282-4286: "max. packet size too big").
    pub const MAX_SIZE: u16 = 4096;

    /// The Paris id of a measurement's `run`-th run (1 for the first).
    /// traceroute_init sets paris to parisbase (traceroute.c:4408) and each
    /// run's traceroute_start2 steps it (traceroute.c:4492-4497):
    /// `paris= (paris-parisbase+1+parismod) % parismod + parisbase`, i.e.
    /// `run % parismod + parisbase`. None without Paris (`-a 0`), where
    /// report() writes no "paris_id" (traceroute.c:428-431).
    pub fn paris_id(&self, run: u64) -> Option<u16> {
        if self.paris_mod == 0 {
            return None;
        }
        Some((run % self.paris_mod as u64) as u16 + self.paris_base as u16)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reference's results in the 2026-10-13 twin run: paris_id 1 with
    /// the default modulus and with `-a 16`, 0 with `-a 1` (25603503); 6052's
    /// second run 2, its UDP probes going to 33436 instead of 33435.
    #[test]
    fn paris_id_steps_once_per_run_from_the_base() {
        let opts = TracerouteOptions::default();
        assert_eq!(opts.paris_id(1), Some(1));
        assert_eq!(opts.paris_id(2), Some(2));
        assert_eq!(opts.paris_id(15), Some(15));
        assert_eq!(opts.paris_id(16), Some(0));
        assert_eq!(opts.paris_id(17), Some(1));

        let one = TracerouteOptions {
            paris_mod: 1,
            ..Default::default()
        };
        assert_eq!(one.paris_id(1), Some(0));
        assert_eq!(one.paris_id(2), Some(0));

        let based = TracerouteOptions {
            paris_mod: 4,
            paris_base: 10,
            ..Default::default()
        };
        assert_eq!(based.paris_id(1), Some(11));
        assert_eq!(based.paris_id(4), Some(10));

        let classic = TracerouteOptions {
            paris_mod: 0,
            ..Default::default()
        };
        assert_eq!(classic.paris_id(1), None);
    }
}
