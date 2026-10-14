//! evtraceroute's packets, byte for byte: the probes send_pkt builds and
//! the way ready_callback4/ready_callback6/ready_tcp4/ready_tcp6 read what
//! comes back (probe-busybox/eperd/traceroute.c, tag 5130). Plain functions
//! over bytes, so every platform builds and tests them; the sockets are in
//! `net`.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::time::Duration;

/// UDP probes go to BASE_PORT + Paris id (or + sequence number without
/// Paris); traceroute.c:44.
pub const BASE_PORT: u16 = 0x8000 + 666;
/// UDP and TCP probes come from SRC_BASE_PORT + the measurement's index;
/// traceroute.c:45 and 4524-4553.
pub const SRC_BASE_PORT: u16 = 20480;
/// What send_pkt strcpy()s into the data after its own fields, cut at the
/// probe's size (traceroute.c:518).
pub const ID_STRING: &[u8] = b"http://atlas.ripe.net Atlas says Hi!";
/// `struct v6info` on a 64-bit host: fuzz, 2 bytes padding, pid, id, seq,
/// then a `struct timespec` at offset 16, already 8-byte aligned
/// (traceroute.c:210-217).
pub const V6INFO_LEN: usize = 32;

/// ICMP header length (ICMP_MINLEN, ICMP6_HDR).
const ICMP_HDR: usize = 8;
/// ready_callback4 subtracts a 20-byte IP header from every size, whatever
/// the header's real length (IPHDR, traceroute.c:41).
const IPHDR: i32 = 20;
const TCP_HDR: usize = 20;

const IPPROTO_HOPOPTS: u8 = 0;
const IPPROTO_ICMP: u8 = 1;
const IPPROTO_TCP: u8 = 6;
const IPPROTO_UDP: u8 = 17;
const IPPROTO_FRAGMENT: u8 = 44;
const IPPROTO_ICMPV6: u8 = 58;
const IPPROTO_DSTOPTS: u8 = 60;

/// One's-complement sum of 16-bit words, as in_cksum adds them
/// (traceroute.c:229-250): an odd last byte is the high byte of a word.
fn sum16(data: &[u8], mut acc: u32) -> u32 {
    let (words, rest) = data.as_chunks::<2>();
    for w in words {
        acc += u16::from_be_bytes(*w) as u32;
    }
    if let [b] = rest {
        acc += (*b as u32) << 8;
    }
    acc
}

/// in_cksum's ending: fold twice, complement.
fn fold_not(acc: u32) -> u16 {
    let s = (acc >> 16) + (acc & 0xffff);
    let s = s + (s >> 16);
    !(s as u16)
}

/// in_cksum (traceroute.c:229).
pub fn in_cksum(data: &[u8]) -> u16 {
    fold_not(sum16(data, 0))
}

/// in_cksum_udp's IPv4 pseudo-header `struct v4_ph` (traceroute.c:192).
fn pseudo4(src: Ipv4Addr, dst: Ipv4Addr, proto: u8, len: u16) -> [u8; 12] {
    let mut ph = [0u8; 12];
    ph[0..4].copy_from_slice(&src.octets());
    ph[4..8].copy_from_slice(&dst.octets());
    ph[9] = proto;
    ph[10..12].copy_from_slice(&len.to_be_bytes());
    ph
}

/// in_cksum_icmp6's IPv6 pseudo-header `struct v6_ph` (traceroute.c:201).
fn pseudo6(src: Ipv6Addr, dst: Ipv6Addr, len: u32, nxt: u8) -> [u8; 40] {
    let mut ph = [0u8; 40];
    ph[0..16].copy_from_slice(&src.octets());
    ph[16..32].copy_from_slice(&dst.octets());
    ph[32..36].copy_from_slice(&len.to_be_bytes());
    ph[39] = nxt;
    ph
}

/// The checksum of `data` behind a pseudo-header.
fn cksum_with(ph: &[u8], data: &[u8]) -> u16 {
    fold_not(sum16(data, sum16(ph, 0)))
}

/// The fudge send_pkt writes into a probe so that its checksum comes out
/// as `val` (`usum= sum + (0xffff - val); sum= usum + (usum >> 16)`,
/// traceroute.c:822-828 and 1177-1181).
fn fudge(sum: u16, val: u16) -> u16 {
    let usum = sum as u32 + (0xffff - val as u32);
    (usum + (usum >> 16)) as u16
}

/// memset the rest of the data to 0 and strcpy the id string at `at`, cut
/// at the data's end (e.g. traceroute.c:994-1000).
fn fill(data: &mut [u8], at: usize) {
    if at >= data.len() {
        return;
    }
    for b in &mut data[at..] {
        *b = 0;
    }
    let n = (data.len() - at).min(ID_STRING.len());
    data[at..at + n].copy_from_slice(&ID_STRING[..n]);
}

/// Whether an IPv4 echo's data (`icmp_data`, what follows the 8-byte ICMP
/// header) is an evtraceroute probe's: after the two Paris bytes, the id
/// string, as far as the packet has it (traceroute.c:1262-1284).
///
/// Not in traceroute.c, which takes any IPv4 echo reply, or quoted echo
/// request, whose id is its index (ready_callback4, 2279-2298 and
/// 2576-2595). There the only other echoes in the process are evping's,
/// with the one id pid & 0x0fff (ping.c:555): they collide with a trace
/// only if that constant is one of the few indexes in use. Starla's ping
/// draws a random 16-bit id per run (ping/icmp.rs), which is a given IPv4
/// ICMP trace's index (top nibble 0, low 12 bits the index) once in 65,536
/// pings: its replies would then be read as that trace's answers, from
/// whatever address the ping went to (N3). The data tells them apart; a
/// probe's own echo reply, or the probe an error quotes, always carries it.
/// IPv6 needs no such check: the v6info's pid and index decide.
fn carries_id_string(data: &[u8]) -> bool {
    let rest = data.get(2..).unwrap_or_default();
    let n = rest.len().min(ID_STRING.len());
    rest[..n] == ID_STRING[..n]
}

/// `struct v6info`, the reference's own header in every IPv6 UDP and ICMP
/// probe: whose (pid, index) it is, its sequence number, and when it went
/// (a CLOCK_MONOTONIC timespec in host byte order).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct V6Info {
    pub fuzz: u16,
    pub pid: u32,
    pub id: u32,
    pub seq: u32,
    pub tv: Duration,
}

impl V6Info {
    fn write(&self, out: &mut [u8]) {
        out[0..2].copy_from_slice(&self.fuzz.to_be_bytes());
        out[2..4].fill(0);
        out[4..8].copy_from_slice(&self.pid.to_be_bytes());
        out[8..12].copy_from_slice(&self.id.to_be_bytes());
        out[12..16].copy_from_slice(&self.seq.to_be_bytes());
        out[16..24].copy_from_slice(&(self.tv.as_secs() as i64).to_ne_bytes());
        out[24..32].copy_from_slice(&(self.tv.subsec_nanos() as i64).to_ne_bytes());
    }

    fn read(b: &[u8]) -> Option<Self> {
        let b = b.get(..V6INFO_LEN)?;
        let sec = i64::from_ne_bytes(b[16..24].try_into().ok()?);
        let nsec = i64::from_ne_bytes(b[24..32].try_into().ok()?);
        let tv = if sec >= 0 && (0..1_000_000_000).contains(&nsec) {
            Duration::new(sec as u64, nsec as u32)
        } else {
            Duration::ZERO
        };
        Some(Self {
            fuzz: u16::from_be_bytes([b[0], b[1]]),
            pid: u32::from_be_bytes(b[4..8].try_into().ok()?),
            id: u32::from_be_bytes(b[8..12].try_into().ok()?),
            seq: u32::from_be_bytes(b[12..16].try_into().ok()?),
            tv,
        })
    }
}

/// An IPv4 ICMP echo request (traceroute.c:1251-1301): id = index (eperd
/// instance 0 in the top four bits), `size` data bytes of which the first
/// two are the Paris fudge that makes the checksum equal the Paris id.
pub fn icmp4_echo(id: u16, seq: u16, size: u16, paris: Option<u16>) -> Vec<u8> {
    // icmp_data[0..2] are always there: curpacksize is at least 2.
    let len = ICMP_HDR + (size as usize).max(2);
    let mut p = vec![0u8; len];
    p[0] = 8;
    p[4..6].copy_from_slice(&id.to_be_bytes());
    p[6..8].copy_from_slice(&seq.to_be_bytes());
    fill(&mut p, ICMP_HDR + 2);
    if let Some(paris) = paris {
        let f = fudge(in_cksum(&p), paris);
        p[8..10].copy_from_slice(&f.to_be_bytes());
    }
    let c = in_cksum(&p);
    p[2..4].copy_from_slice(&c.to_be_bytes());
    p
}

/// An IPv6 ICMP echo request (traceroute.c:756-828): id = the pid's low 16
/// bits, then a v6info and `size` data bytes in all (at least the v6info).
/// With Paris the v6info's fuzz makes the checksum paris+1. The checksum
/// field stays 0: the kernel fills it in on a raw ICMPv6 socket, over the
/// same pseudo-header (`src` must be the address the probe leaves from).
#[allow(clippy::too_many_arguments)]
pub fn icmp6_echo(
    src: Ipv6Addr,
    dst: Ipv6Addr,
    pid: u32,
    index: u16,
    seq: u16,
    tv: Duration,
    size: u16,
    paris: Option<u16>,
) -> Vec<u8> {
    let len = ICMP_HDR + (size as usize).max(V6INFO_LEN);
    let mut p = vec![0u8; len];
    p[0] = 128;
    p[4..6].copy_from_slice(&(pid as u16).to_be_bytes());
    p[6..8].copy_from_slice(&seq.to_be_bytes());
    let mut info = V6Info {
        fuzz: 0,
        pid,
        id: index as u32,
        seq: seq as u32,
        tv,
    };
    info.write(&mut p[ICMP_HDR..]);
    fill(&mut p, ICMP_HDR + V6INFO_LEN);
    if let Some(paris) = paris {
        let ph = pseudo6(src, dst, len as u32, IPPROTO_ICMPV6);
        // "Avoid 0" (traceroute.c:821)
        info.fuzz = fudge(cksum_with(&ph, &p), paris.wrapping_add(1));
        info.write(&mut p[ICMP_HDR..]);
    }
    p
}

/// The checksum the kernel puts into an ICMPv6 probe.
pub fn icmp6_cksum(src: Ipv6Addr, dst: Ipv6Addr, packet: &[u8]) -> u16 {
    let ph = pseudo6(src, dst, packet.len() as u32, IPPROTO_ICMPV6);
    let mut p = packet.to_vec();
    p[2..4].fill(0);
    cksum_with(&ph, &p)
}

/// An IPv6 UDP probe's data (traceroute.c:975-1000): a v6info, then the id
/// string up to `size` bytes (at least the v6info).
pub fn udp6_payload(pid: u32, index: u16, seq: u16, tv: Duration, size: u16) -> Vec<u8> {
    let mut p = vec![0u8; (size as usize).max(V6INFO_LEN)];
    V6Info {
        fuzz: 0,
        pid,
        id: index as u32,
        seq: seq as u32,
        tv,
    }
    .write(&mut p);
    fill(&mut p, V6INFO_LEN);
    p
}

/// An IPv4 UDP probe's data (traceroute.c:1431-1487): two fudge bytes that
/// make the UDP checksum `val` (the sequence number with Paris, else
/// index+1), then the id string up to `size` bytes (at least 2).
pub fn udp4_payload(
    src: Ipv4Addr,
    dst: Ipv4Addr,
    sport: u16,
    dport: u16,
    size: u16,
    val: u16,
) -> Vec<u8> {
    let mut p = vec![0u8; (size as usize).max(2)];
    fill(&mut p, 2);
    let ulen = (8 + p.len()) as u16;
    let mut hdr = [0u8; 8];
    hdr[0..2].copy_from_slice(&sport.to_be_bytes());
    hdr[2..4].copy_from_slice(&dport.to_be_bytes());
    hdr[4..6].copy_from_slice(&ulen.to_be_bytes());
    let ph = pseudo4(src, dst, IPPROTO_UDP, ulen);
    let sum = fold_not(sum16(&p, sum16(&hdr, sum16(&ph, 0))));
    p[0..2].copy_from_slice(&fudge(sum, val).to_be_bytes());
    p
}

/// The UDP checksum the kernel puts on an IPv4 probe.
pub fn udp4_cksum(src: Ipv4Addr, dst: Ipv4Addr, sport: u16, dport: u16, payload: &[u8]) -> u16 {
    let ulen = (8 + payload.len()) as u16;
    let mut hdr = [0u8; 8];
    hdr[0..2].copy_from_slice(&sport.to_be_bytes());
    hdr[2..4].copy_from_slice(&dport.to_be_bytes());
    hdr[4..6].copy_from_slice(&ulen.to_be_bytes());
    let ph = pseudo4(src, dst, IPPROTO_UDP, ulen);
    fold_not(sum16(payload, sum16(&hdr, sum16(&ph, 0))))
}

/// A TCP SYN probe (traceroute.c:588-688 and 1065-1157): sequence number
/// index << 16 | seq, no options, window 0, then `size` data bytes, checksum
/// over the pseudo-header.
pub fn tcp_syn(
    src: IpAddr,
    dst: IpAddr,
    sport: u16,
    dport: u16,
    index: u16,
    seq: u16,
    size: u16,
) -> Vec<u8> {
    let mut p = vec![0u8; TCP_HDR + size as usize];
    p[0..2].copy_from_slice(&sport.to_be_bytes());
    p[2..4].copy_from_slice(&dport.to_be_bytes());
    p[4..8].copy_from_slice(&((index as u32) << 16 | seq as u32).to_be_bytes());
    p[12] = ((TCP_HDR / 4) as u8) << 4;
    p[13] = 0x02; // SYN
    fill(&mut p, TCP_HDR);
    let c = match (src, dst) {
        (IpAddr::V4(s), IpAddr::V4(d)) => {
            cksum_with(&pseudo4(s, d, IPPROTO_TCP, p.len() as u16), &p)
        }
        (IpAddr::V6(s), IpAddr::V6(d)) => {
            cksum_with(&pseudo6(s, d, p.len() as u32, IPPROTO_TCP), &p)
        }
        _ => 0,
    };
    p[16..18].copy_from_slice(&c.to_be_bytes());
    p
}

/// Whose probes a reply may answer: what the reference compares a reply's
/// quoted packet with before it looks at the sequence number.
#[derive(Debug, Clone)]
pub struct TraceId {
    pub v6: bool,
    pub protocol: starla_common::TracerouteProtocol,
    /// eperd's getpid(): in every v6info, and the IPv6 echo id
    pub pid: u32,
    /// the measurement's slot (trtstate.index)
    pub index: u16,
    pub target: IpAddr,
    pub paris: Option<u16>,
}

/// Which of the reference's reply handlers would take the packet; each
/// writes its reply's fields in its own order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Branch {
    /// ready_callback4, ICMP error quoting a TCP or UDP probe
    V4ErrL4,
    /// ready_callback4, ICMP error quoting an ICMP echo
    V4ErrIcmp,
    /// ready_callback4, echo reply
    V4Echo,
    /// ready_tcp4
    V4Tcp,
    /// ready_callback6, any ICMPv6 error
    V6Err,
    /// ready_callback6, echo reply
    V6Echo,
    /// ready_tcp6
    V6Tcp,
}

/// What a reply says about the path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    TimeExceeded,
    /// destination unreachable, with its code (IPv4 code 4 is `NeedFrag`)
    Unreach(u8),
    /// IPv4 fragmentation needed, with the next-hop MTU
    NeedFrag(u32),
    /// ICMPv6 packet too big, with the MTU
    TooBig(u32),
    EchoReply,
    /// a TCP answer from the target (SYN-ACK or RST)
    Tcp,
}

/// A reply to one of this trace's probes, with everything the reference
/// writes about it.
#[derive(Debug, Clone, PartialEq)]
pub struct Answer {
    pub branch: Branch,
    pub outcome: Outcome,
    /// The sequence number the reply carries: the quoted probe's, or for a
    /// TCP answer the low 16 bits of its acknowledgment number.
    pub seq: u32,
    pub from: IpAddr,
    /// TTL / hop limit the reply arrived with (-42 when the kernel said
    /// nothing, as the reference initialises rcvdttl)
    pub ttl: i32,
    pub size: i32,
    /// IPv4: the reply's TOS byte; IPv6: its traffic class
    pub tos: i32,
    /// the quoted probe's remaining TTL / hop limit
    pub inner_ttl: u8,
    /// the quoted probe's TOS / traffic class
    pub inner_tos: u8,
    /// IPv4: the quoted probe's destination when it isn't the target
    pub edst: Option<Ipv4Addr>,
    pub hbh_opt_size: u32,
    pub dst_opt_size: u32,
    /// IPv4 ICMP errors: what the quoted IPv4 header's protocol takes off an
    /// MTU to get the data size (8 for UDP and ICMP, 20 for TCP)
    pub inner_l4_len: u32,
    /// TCP answers: "FSRPAU" letters
    pub flags: String,
    /// TCP answers: the rendered `, "hdropts": [ … ]`
    pub hdropts: Option<String>,
    /// ICMP errors: the rendered `, "icmpext": { … }`
    pub icmpext: Option<String>,
    /// IPv6: the probe's send time from the quoted v6info, for late replies
    pub sent_tv: Option<Duration>,
}

impl Answer {
    pub fn new(branch: Branch, outcome: Outcome, seq: u32, from: IpAddr) -> Self {
        Self {
            branch,
            outcome,
            seq,
            from,
            ttl: -42,
            size: 0,
            tos: 0,
            inner_ttl: 1,
            inner_tos: 0,
            edst: None,
            hbh_opt_size: 0,
            dst_opt_size: 0,
            inner_l4_len: 8,
            flags: String::new(),
            hdropts: None,
            icmpext: None,
            sent_tv: None,
        }
    }
}

fn be16(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes(b.get(at..at + 2)?.try_into().ok()?))
}

fn be32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

/// do_mpls (traceroute.c:1577-1602).
fn mpls(out: &mut String, p: &[u8]) {
    use std::fmt::Write;
    out.push_str(", \"mpls\": [");
    let mut o = 0;
    while o + 4 <= p.len() {
        let v = u32::from_be_bytes([p[o], p[o + 1], p[o + 2], p[o + 3]]);
        let _ = write!(
            out,
            "{} {{ \"label\":{}, \"exp\":{}, \"s\":{}, \"ttl\":{} }}",
            if o == 0 { "" } else { "," },
            v >> 12,
            (v >> 9) & 0x7,
            u8::from(v & 0x100 != 0),
            v & 0xff
        );
        o += 4;
    }
    out.push_str(" ]");
}

/// do_icmp_multi (traceroute.c:1604-1667): RFC 4884 extensions, or the
/// pre-RFC 4884 kind after 128 bytes of quoted packet; nothing at all when
/// the extension's checksum is wrong.
pub fn icmp_multi(p: &[u8], pre_rfc4884: bool) -> Option<String> {
    use std::fmt::Write;
    if p.len() < 4 || in_cksum(p) != 0 {
        return None;
    }
    let mut out = String::new();
    let _ = write!(
        out,
        ", \"icmpext\": {{ \"version\":{}, \"rfc4884\":{}",
        p[0] >> 4,
        u8::from(!pre_rfc4884)
    );
    out.push_str(", \"obj\": [");
    let mut o = 4;
    while o + 4 < p.len() {
        let len = u16::from_be_bytes([p[o], p[o + 1]]) as usize;
        let (class, ctype) = (p[o + 2], p[o + 3]);
        let _ = write!(
            out,
            "{} {{ \"class\":{}, \"type\":{}",
            if o == 4 { "" } else { "," },
            class,
            ctype
        );
        if len < 4 || o + len > p.len() {
            out.push_str(" }");
            break;
        }
        if class == 1 && ctype == 1 {
            mpls(&mut out, &p[o + 4..o + len]);
        }
        o += len;
        out.push_str(" }");
    }
    out.push_str(" ] }");
    Some(out)
}

/// report_hdropts (traceroute.c:2722-2760): MSS options, then the first
/// other option's kind, and stop.
fn hdropts(opts: &[u8]) -> String {
    use std::fmt::Write;
    let mut out = String::from(", \"hdropts\": [ ");
    let mut s = 0;
    while s < opts.len() {
        let sep = if s != 0 { ", " } else { "" };
        match opts[s] {
            2 => {
                let len = opts.get(s + 1).copied().unwrap_or(0) as usize;
                if len < 4 || s + len > opts.len() {
                    break;
                }
                let mss = u16::from_be_bytes([opts[s + 2], opts[s + 3]]);
                let _ = write!(out, "{}{{ \"mss\":{} }}", sep, mss);
                s += len;
                continue;
            }
            o => {
                let _ = write!(out, "{}{{ \"unknown-opt\":{} }}", sep, o);
            }
        }
        break;
    }
    out.push_str(" ]");
    out
}

/// The "FSRPAU" letters ready_tcp4/6 write for a TCP answer's flags.
fn tcp_flags(b: u8) -> String {
    let mut s = String::new();
    for (bit, c) in [
        (0x01, 'F'),
        (0x02, 'S'),
        (0x04, 'R'),
        (0x08, 'P'),
        (0x10, 'A'),
        (0x20, 'U'),
    ] {
        if b & bit != 0 {
            s.push(c);
        }
    }
    s
}

/// An IPv4 ICMP packet, IP header first, as a raw socket returns it:
/// ready_callback4 (traceroute.c:1669-2720). `cur_seq` is the sequence
/// number of the probe in flight (a Paris UDP probe's quoted checksum is
/// clamped to it, traceroute.c:2087-2098).
pub fn parse_icmp4(id: &TraceId, buf: &[u8], from: Ipv4Addr, cur_seq: u16) -> Option<Answer> {
    use starla_common::TracerouteProtocol as P;

    let nrecv = buf.len() as i32;
    let ihl = (*buf.first()? & 0x0f) as usize;
    let hlen = ihl * 4;
    if buf.len() < hlen + ICMP_HDR || ihl < 5 {
        return None;
    }
    let (ttl, tos) = (buf[8] as i32, buf[1] as i32);
    let icmp = &buf[hlen..];
    let from_ip = IpAddr::V4(from);

    match icmp[0] {
        // ICMP_TIME_EXCEEDED, ICMP_DEST_UNREACH
        11 | 3 => {
            let eip = &icmp[ICMP_HDR..];
            let ehl = (*eip.first()? & 0x0f) as usize;
            let ehlen = ehl * 4;
            if eip.len() < ehlen || ehl < 5 {
                return None;
            }
            let l4 = &eip[ehlen..];
            let (branch, seq, l4_len) = match eip[9] {
                IPPROTO_TCP if id.protocol == P::TCP => {
                    if l4.len() < 8 {
                        return None;
                    }
                    let sport = be16(l4, 0)?;
                    let tseq = be32(l4, 4)?;
                    if sport != SRC_BASE_PORT.wrapping_add(id.index)
                        || (tseq >> 16) as u16 != id.index
                    {
                        return None;
                    }
                    (Branch::V4ErrL4, tseq & 0xffff, TCP_HDR as u32)
                }
                IPPROTO_UDP if id.protocol == P::UDP => {
                    if l4.len() < 8 {
                        return None;
                    }
                    let sport = be16(l4, 0)?;
                    if sport != SRC_BASE_PORT.wrapping_add(id.index) {
                        return None;
                    }
                    let seq = if id.paris.is_some() {
                        // The sequence number is the checksum; NAT boxes
                        // that forget to restore it get the benefit of the
                        // doubt.
                        (be16(l4, 6)?).min(cur_seq) as u32
                    } else {
                        // dport - BASE_PORT in unsigned arithmetic: below
                        // the base is "from the future", and ignored.
                        (be16(l4, 2)? as u32).wrapping_sub(BASE_PORT as u32)
                    };
                    (Branch::V4ErrL4, seq, 8)
                }
                IPPROTO_ICMP if id.protocol == P::ICMP => {
                    if l4.len() < 8 || l4[0] != 8 || l4[1] != 0 {
                        return None;
                    }
                    let eid = be16(l4, 4)?;
                    if eid >> 12 != 0 || eid & 0x0fff != id.index || !carries_id_string(&l4[8..]) {
                        return None;
                    }
                    (Branch::V4ErrIcmp, be16(l4, 6)? as u32, 8)
                }
                _ => return None,
            };
            let outcome = match (icmp[0], icmp[1]) {
                (11, _) => Outcome::TimeExceeded,
                (_, 4) => Outcome::NeedFrag(be16(icmp, 6)? as u32),
                (_, code) => Outcome::Unreach(code),
            };
            let mut a = Answer::new(branch, outcome, seq, from_ip);
            a.ttl = ttl;
            a.tos = tos;
            a.size = nrecv - IPHDR - ICMP_HDR as i32;
            a.inner_ttl = eip[8];
            a.inner_tos = eip[1];
            a.inner_l4_len = l4_len;
            let edst = Ipv4Addr::new(eip[16], eip[17], eip[18], eip[19]);
            if IpAddr::V4(edst) != id.target {
                a.edst = Some(edst);
            }
            // RFC 4884: the length of the original datagram, in 32-bit
            // words, is the low byte of icmp_pmvoid.
            let prefix = icmp[5] as usize * 4;
            a.icmpext = if prefix != 0 {
                let off = hlen + ICMP_HDR + prefix;
                if buf.len() > off {
                    icmp_multi(&buf[off..], false)
                } else {
                    None
                }
            } else if buf.len() > hlen + ICMP_HDR + 128 {
                icmp_multi(&buf[hlen + ICMP_HDR + 128..], true)
            } else {
                None
            };
            Some(a)
        }
        // ICMP_ECHOREPLY
        0 if id.protocol == P::ICMP => {
            if icmp[1] != 0 {
                return None;
            }
            let rid = be16(icmp, 4)?;
            if rid >> 12 != 0 || rid & 0x0fff != id.index || !carries_id_string(&icmp[ICMP_HDR..]) {
                return None;
            }
            let mut a = Answer::new(
                Branch::V4Echo,
                Outcome::EchoReply,
                be16(icmp, 6)? as u32,
                from_ip,
            );
            a.ttl = ttl;
            a.tos = tos;
            a.size = nrecv - IPHDR - ICMP_HDR as i32;
            Some(a)
        }
        _ => None,
    }
}

/// An ICMPv6 message as a raw ICMPv6 socket returns it (no IPv6 header),
/// with the hop limit and traffic class from its ancillary data:
/// ready_callback6 (traceroute.c:3260-4147).
pub fn parse_icmp6(
    id: &TraceId,
    buf: &[u8],
    from: Ipv6Addr,
    hoplimit: i32,
    tclass: i32,
) -> Option<Answer> {
    use starla_common::TracerouteProtocol as P;

    let nrecv = buf.len();
    if nrecv < ICMP_HDR {
        return None;
    }
    let from_ip = IpAddr::V6(from);
    match buf[0] {
        // ICMP6_DST_UNREACH, ICMP6_PACKET_TOO_BIG, ICMP6_TIME_EXCEEDED
        1..=3 => {
            if nrecv < ICMP_HDR + 40 {
                return None;
            }
            let eip = &buf[ICMP_HDR..];
            let mut nxt = eip[6];
            if !matches!(
                nxt,
                IPPROTO_FRAGMENT
                    | IPPROTO_HOPOPTS
                    | IPPROTO_DSTOPTS
                    | IPPROTO_TCP
                    | IPPROTO_UDP
                    | IPPROTO_ICMPV6
            ) {
                return None;
            }
            let mut ptr = ICMP_HDR + 40;
            let (mut hbh, mut dst) = (0u32, 0u32);
            if nxt == IPPROTO_HOPOPTS {
                if ptr + 2 > nrecv {
                    return None;
                }
                hbh = 8 * buf[ptr + 1] as u32;
                let optlen = hbh as usize + 8;
                if ptr + optlen > nrecv {
                    return None;
                }
                nxt = buf[ptr];
                ptr += optlen;
            }
            if nxt == IPPROTO_FRAGMENT {
                if ptr + 8 > nrecv {
                    return None;
                }
                // Not the first fragment: nothing to match.
                if be16(buf, ptr + 2)? & !3 != 0 {
                    return None;
                }
                nxt = buf[ptr];
                ptr += 8;
            }
            if nxt == IPPROTO_DSTOPTS {
                if ptr + 2 > nrecv {
                    return None;
                }
                dst = 8 * buf[ptr + 1] as u32;
                let optlen = dst as usize + 8;
                if ptr + optlen > nrecv {
                    return None;
                }
                nxt = buf[ptr];
                ptr += optlen;
            }
            let (siz, info_siz) = match nxt {
                IPPROTO_TCP => (TCP_HDR, 0),
                _ => (8, V6INFO_LEN),
            };
            if ptr + siz + info_siz > nrecv {
                return None;
            }
            let quoted = match nxt {
                IPPROTO_TCP => P::TCP,
                IPPROTO_UDP => P::UDP,
                _ => P::ICMP,
            };
            let (seq, info) = if nxt == IPPROTO_TCP {
                let tseq = be32(buf, ptr + 4)?;
                if (tseq >> 16) as u16 != id.index {
                    return None;
                }
                (tseq & 0xffff, None)
            } else {
                let info = V6Info::read(&buf[ptr + 8..])?;
                if info.pid != id.pid || info.id != id.index as u32 {
                    return None;
                }
                (info.seq, Some(info))
            };
            if quoted != id.protocol {
                return None;
            }
            let outcome = match buf[0] {
                3 => Outcome::TimeExceeded,
                2 => Outcome::TooBig(be32(buf, 4)?),
                _ => Outcome::Unreach(buf[1]),
            };
            let mut a = Answer::new(Branch::V6Err, outcome, seq, from_ip);
            a.ttl = hoplimit;
            a.tos = tclass;
            a.size = nrecv as i32 - ICMP_HDR as i32;
            a.inner_ttl = eip[7];
            a.inner_tos = ((be32(eip, 0)? >> 20) & 0xff) as u8;
            a.hbh_opt_size = hbh;
            a.dst_opt_size = dst;
            a.inner_l4_len = siz as u32;
            a.sent_tv = info.map(|i| i.tv);
            // RFC 4884: the original datagram's length in 8-byte units.
            let prefix = buf[4] as usize * 8;
            a.icmpext = if prefix != 0 {
                let off = ICMP_HDR + prefix;
                if nrecv > off {
                    icmp_multi(&buf[off..], false)
                } else {
                    None
                }
            } else if nrecv > ICMP_HDR + 128 {
                icmp_multi(&buf[ICMP_HDR + 128..], true)
            } else {
                None
            };
            Some(a)
        }
        // ICMP6_ECHO_REPLY
        129 if id.protocol == P::ICMP => {
            let info = V6Info::read(buf.get(ICMP_HDR..)?)?;
            if info.pid != id.pid || info.id != id.index as u32 {
                return None;
            }
            let mut a = Answer::new(Branch::V6Echo, Outcome::EchoReply, info.seq, from_ip);
            a.ttl = hoplimit;
            a.tos = tclass;
            a.size = nrecv as i32 - ICMP_HDR as i32;
            a.sent_tv = Some(info.tv);
            Some(a)
        }
        _ => None,
    }
}

/// A TCP segment from the target as a raw TCP socket returns it (IPv4:
/// IP header first; IPv6: the segment, hop limit and traffic class from the
/// ancillary data): ready_tcp4 (traceroute.c:2762-2982) and ready_tcp6
/// (2984-3258).
pub fn parse_tcp(
    id: &TraceId,
    buf: &[u8],
    from: IpAddr,
    hoplimit: i32,
    tclass: i32,
) -> Option<Answer> {
    let (tcp, ttl, tos, size, branch) = match from {
        IpAddr::V4(_) => {
            let ihl = (*buf.first()? & 0x0f) as usize;
            let hlen = ihl * 4;
            if buf.len() < hlen + TCP_HDR || ihl < 5 {
                return None;
            }
            (
                &buf[hlen..],
                buf[8] as i32,
                buf[1] as i32,
                buf.len() as i32 - IPHDR - TCP_HDR as i32,
                Branch::V4Tcp,
            )
        }
        IpAddr::V6(_) => (
            buf,
            hoplimit,
            tclass,
            buf.len() as i32 - TCP_HDR as i32,
            Branch::V6Tcp,
        ),
    };
    if tcp.len() < TCP_HDR {
        return None;
    }
    let doff = (tcp[12] >> 4) as usize;
    let tcp_hlen = doff * 4;
    if tcp.len() < tcp_hlen || doff < 5 {
        return None;
    }
    // Our port, and our index in the acknowledgment's high 16 bits.
    let ack = be32(tcp, 8)?;
    if be16(tcp, 2)? != SRC_BASE_PORT.wrapping_add(id.index) || (ack >> 16) as u16 != id.index {
        return None;
    }
    let mut a = Answer::new(branch, Outcome::Tcp, ack & 0xffff, from);
    a.ttl = ttl;
    a.tos = tos;
    a.size = size;
    a.flags = tcp_flags(tcp[13]);
    if tcp_hlen > TCP_HDR {
        a.hdropts = Some(hdropts(&tcp[TCP_HDR..tcp_hlen]));
    }
    Some(a)
}

/// A packet off one of a run's sockets, read as the handler eperd has on
/// that socket would read it: the raw TCP socket's by ready_tcp4/6, the
/// ICMP socket's by ready_callback4 or ready_callback6. A run's ICMP socket
/// hears every ICMP message to this host — other measurements' echo
/// replies, errors quoting other processes' packets, neighbour discovery —
/// so None (not this run's) is the usual answer. `cur_seq` is the sequence
/// number of the probe in flight.
pub fn parse_reply(
    id: &TraceId,
    from_tcp_socket: bool,
    buf: &[u8],
    from: IpAddr,
    hoplimit: i32,
    tclass: i32,
    cur_seq: u16,
) -> Option<Answer> {
    if from_tcp_socket {
        return parse_tcp(id, buf, from, hoplimit, tclass);
    }
    match from {
        IpAddr::V4(from) => parse_icmp4(id, buf, from, cur_seq),
        IpAddr::V6(from) => parse_icmp6(id, buf, from, hoplimit, tclass),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use starla_common::TracerouteProtocol as P;

    fn hex(s: &str) -> Vec<u8> {
        let s: String = s.split_whitespace().collect();
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    const REF: Ipv6Addr = Ipv6Addr::new(0x2001, 0xdb8, 0x7769, 0x6e00, 0, 0, 0, 0x31);
    const DST: Ipv6Addr = Ipv6Addr::new(0x2001, 0xdb8, 0x7769, 0x6e00, 0, 0, 0, 0x20);

    /// The reference's first UDP probe of msm 6052 (topology6) in the
    /// 2026-10-13 twin run (probe-ref.pcap, 1791871194.82):
    /// 2001:db8:7769:6e00::31.20493 > ::20.33435, `-S 40`, pid 52, index
    /// 13, seq 1, Paris id 1.
    const REF_UDP6_PROBE: &str = "0000 0000 0000 0034 0000 000d 0000 0001
        1d09 0000 0000 0000 9ee4 7b19 0000 0000 6874 7470 3a2f 2f61";

    #[test]
    fn udp6_payload_is_the_references_v6info_and_id_string() {
        let tv = Duration::new(0x091d, 0x197b_e49e);
        let p = udp6_payload(52, 13, 1, tv, 40);
        assert_eq!(p, hex(REF_UDP6_PROBE));
        // The ports: 20480 + index, 33434 + Paris id 1.
        assert_eq!(SRC_BASE_PORT + 13, 20493);
        assert_eq!(BASE_PORT + 1, 33435);
        // Smaller than a v6info is a v6info.
        assert_eq!(udp6_payload(52, 13, 1, tv, 0).len(), V6INFO_LEN);
    }

    /// The reference's ICMPv6 echo request of msm 222017431 (probe-ref.pcap,
    /// 1791871131.89): id 55 (pid), seq 1, `-S 48`, Paris id 1, so the
    /// checksum is 0x0002.
    #[test]
    fn icmp6_echo_is_the_references_with_the_paris_checksum() {
        let wire = hex("8000 0002 0037 0001 57f8 0000 0000 0037 0000 0000 0000 0001
             de08 0000 0000 0000 d32d 9b1d 0000 0000 6874 7470 3a2f 2f61
             746c 6173 2e72 6970");
        let tv = Duration::new(0x08de, 0x1d9b_2dd3);
        let p = icmp6_echo(REF, DST, 55, 0, 1, tv, 48, Some(1));
        assert_eq!(p.len(), 56);
        assert_eq!(
            &p[4..],
            &wire[4..],
            "header after the checksum, v6info, data"
        );
        assert_eq!(icmp6_cksum(REF, DST, &p), 0x0002);
        // Without Paris the fuzz stays 0 and the checksum is whatever it is.
        let classic = icmp6_echo(REF, DST, 55, 0, 1, tv, 48, None);
        assert_eq!(&classic[8..10], &[0, 0]);
    }

    #[test]
    fn icmp4_echo_checksum_is_the_paris_id() {
        for paris in [1u16, 2, 15, 0x1234] {
            for seq in [1u16, 2, 3, 77] {
                let p = icmp4_echo(5, seq, 40, Some(paris));
                assert_eq!(p.len(), 48);
                assert_eq!(u16::from_be_bytes([p[2], p[3]]), paris, "seq {seq}");
                assert_eq!(in_cksum(&p), 0, "a valid checksum");
                assert_eq!(&p[0..2], &[8, 0]);
                assert_eq!(&p[4..8], &[0, 5, (seq >> 8) as u8, seq as u8]);
                assert_eq!(&p[10..18], b"http://a");
            }
        }
        // -S 0: still the two fudge bytes.
        assert_eq!(icmp4_echo(5, 1, 0, None).len(), 10);
    }

    #[test]
    fn udp4_checksum_is_the_sequence_number_with_paris() {
        let (s, d) = (Ipv4Addr::new(192, 0, 2, 1), Ipv4Addr::new(198, 51, 100, 7));
        for val in [1u16, 2, 3, 0x7fff] {
            let p = udp4_payload(s, d, 20480, BASE_PORT + 1, 40, val);
            assert_eq!(p.len(), 40);
            assert_eq!(udp4_cksum(s, d, 20480, BASE_PORT + 1, &p), val);
            assert_eq!(&p[2..10], b"http://a");
        }
    }

    #[test]
    fn tcp_syn_layout() {
        let (s, d) = (Ipv4Addr::new(192, 0, 2, 1), Ipv4Addr::new(198, 51, 100, 7));
        let p = tcp_syn(s.into(), d.into(), 20483, 80, 3, 9, 0);
        assert_eq!(p.len(), 20);
        assert_eq!(&p[0..4], &[0x50, 0x03, 0x00, 0x50]);
        assert_eq!(&p[4..8], &[0, 3, 0, 9]);
        assert_eq!(p[12], 0x50);
        assert_eq!(p[13], 0x02);
        assert_eq!(&p[14..16], &[0, 0], "window 0");
        let ph = pseudo4(s, d, IPPROTO_TCP, 20);
        assert_eq!(cksum_with(&ph, &p), 0);
        assert_eq!(tcp_syn(s.into(), d.into(), 20483, 80, 3, 9, 4).len(), 24);
    }

    fn id6(protocol: P) -> TraceId {
        TraceId {
            v6: true,
            protocol,
            pid: 52,
            index: 13,
            target: DST.into(),
            paris: Some(1),
        }
    }

    /// The destination's port unreachable for that probe (probe-ref.pcap,
    /// 1791871194.82), ICMPv6 part: 96 bytes, "size":88 in the result.
    fn ref_port_unreachable() -> Vec<u8> {
        let mut icmp = hex("0104 5550 0000 0000");
        icmp.extend(hex("600c 7336 0030 1101 2001 0db8 7769 6e00 0000 0000 \
                         0000 0031
             2001 0db8 7769 6e00 0000 0000 0000 0020 500d 829b 0030 26d8"));
        icmp.extend(hex(REF_UDP6_PROBE));
        icmp
    }

    #[test]
    fn a_port_unreachable_from_the_target_answers_the_udp6_probe() {
        let buf = ref_port_unreachable();
        assert_eq!(buf.len(), 96);
        let a = parse_icmp6(&id6(P::UDP), &buf, DST, 64, 0).expect("ours");
        assert_eq!(a.branch, Branch::V6Err);
        assert_eq!(a.outcome, Outcome::Unreach(4));
        assert_eq!(a.seq, 1);
        assert_eq!(a.ttl, 64);
        assert_eq!(a.size, 88);
        assert_eq!(a.inner_ttl, 1);
        assert_eq!(a.inner_tos, 0);
        assert_eq!(a.icmpext, None);
        assert_eq!(a.sent_tv, Some(Duration::new(0x091d, 0x197b_e49e)));

        // Someone else's: another pid, another index, another protocol.
        let mut other = id6(P::UDP);
        other.pid = 53;
        assert_eq!(parse_icmp6(&other, &buf, DST, 64, 0), None);
        let mut other = id6(P::UDP);
        other.index = 12;
        assert_eq!(parse_icmp6(&other, &buf, DST, 64, 0), None);
        assert_eq!(parse_icmp6(&id6(P::ICMP), &buf, DST, 64, 0), None);
        // Not enough of the probe quoted to see its v6info.
        assert_eq!(parse_icmp6(&id6(P::UDP), &buf[..87], DST, 64, 0), None);
        // Neighbour discovery and the like are nobody's.
        assert_eq!(
            parse_icmp6(&id6(P::UDP), &hex("8700 0000 0000 0000"), DST, 64, 0),
            None
        );
    }

    #[test]
    fn an_icmp6_echo_reply_answers_by_its_v6info() {
        let tv = Duration::new(7, 8);
        let mut reply = icmp6_echo(DST, REF, 52, 13, 4, tv, 40, Some(1));
        reply[0] = 129;
        let a = parse_icmp6(&id6(P::ICMP), &reply, DST, 64, 0).unwrap();
        assert_eq!(a.branch, Branch::V6Echo);
        assert_eq!(a.seq, 4);
        assert_eq!(a.size, 40);
        assert_eq!(a.sent_tv, Some(tv));
        assert_eq!(parse_icmp6(&id6(P::UDP), &reply, DST, 64, 0), None);
    }

    fn ipv4(proto: u8, ttl: u8, src: Ipv4Addr, dst: Ipv4Addr, payload: &[u8]) -> Vec<u8> {
        let mut ip = vec![0x45, 0, 0, 0, 0, 0, 0, 0, ttl, proto, 0, 0];
        ip[2..4].copy_from_slice(&((20 + payload.len()) as u16).to_be_bytes());
        ip.extend(src.octets());
        ip.extend(dst.octets());
        ip.extend(payload);
        ip
    }

    #[test]
    fn ipv4_errors_and_echo_replies() {
        let me = Ipv4Addr::new(192, 0, 2, 1);
        let target = Ipv4Addr::new(198, 51, 100, 7);
        let router = Ipv4Addr::new(203, 0, 113, 9);
        let id = TraceId {
            v6: false,
            protocol: P::UDP,
            pid: 1,
            index: 2,
            target: target.into(),
            paris: Some(5),
        };
        // A Paris UDP probe with sequence number 3 (its checksum), quoted
        // whole in a time exceeded with an inner TTL of 1.
        let payload = udp4_payload(me, target, 20482, BASE_PORT + 5, 40, 3);
        let mut udp = vec![0x50, 0x02, 0x82, 0x9f, 0, 48, 0, 0];
        udp[6..8]
            .copy_from_slice(&udp4_cksum(me, target, 20482, BASE_PORT + 5, &payload).to_be_bytes());
        udp.extend(&payload);
        let probe = ipv4(17, 1, me, target, &udp);
        let mut icmp = vec![11, 0, 0, 0, 0, 0, 0, 0];
        icmp.extend(&probe);
        let pkt = ipv4(1, 250, router, me, &icmp);
        let a = parse_icmp4(&id, &pkt, router, 3).unwrap();
        assert_eq!(a.branch, Branch::V4ErrL4);
        assert_eq!(a.outcome, Outcome::TimeExceeded);
        assert_eq!(a.seq, 3);
        assert_eq!(a.ttl, 250);
        // nrecv - 20 - 8: the quoted probe.
        assert_eq!(a.size, 68);
        assert_eq!(a.edst, None);
        // A NAT that left the checksum alone: clamped to the probe in flight.
        assert_eq!(parse_icmp4(&id, &pkt, router, 2).unwrap().seq, 2);
        // Without Paris the sequence number is the port's offset.
        let classic = TraceId {
            paris: None,
            ..id.clone()
        };
        assert_eq!(parse_icmp4(&classic, &pkt, router, 9).unwrap().seq, 5);

        // A port unreachable at the target, with an RFC 4884 MPLS stack.
        let mut icmp = vec![3, 3, 0, 0, 0, 32, 0, 0];
        let mut quoted = probe.clone();
        quoted.resize(128, 0);
        icmp.extend(&quoted);
        let mut ext = vec![0x20, 0, 0, 0, 0, 8, 1, 1, 0x00, 0x01, 0x01, 0x01];
        let c = in_cksum(&ext);
        ext[2..4].copy_from_slice(&c.to_be_bytes());
        icmp.extend(&ext);
        let pkt = ipv4(1, 61, target, me, &icmp);
        let a = parse_icmp4(&id, &pkt, target, 3).unwrap();
        assert_eq!(a.outcome, Outcome::Unreach(3));
        assert_eq!(
            a.icmpext.as_deref(),
            Some(
                ", \"icmpext\": { \"version\":2, \"rfc4884\":1, \"obj\": [ { \"class\":1, \
                 \"type\":1, \"mpls\": [ { \"label\":16, \"exp\":0, \"s\":1, \"ttl\":1 } ] } ] }"
            )
        );

        // An ICMP trace's echo reply: id = index.
        let icmp_id = TraceId {
            protocol: P::ICMP,
            ..id.clone()
        };
        let mut echo = icmp4_echo(2, 7, 40, Some(5));
        echo[0] = 0;
        let pkt = ipv4(1, 57, target, me, &echo);
        let a = parse_icmp4(&icmp_id, &pkt, target, 7).unwrap();
        assert_eq!(a.branch, Branch::V4Echo);
        assert_eq!((a.seq, a.ttl, a.size), (7, 57, 40));
        assert_eq!(parse_icmp4(&id, &pkt, target, 7), None, "not a UDP trace's");
    }

    #[test]
    fn tcp_answers() {
        let me: IpAddr = Ipv6Addr::LOCALHOST.into();
        let id = TraceId {
            v6: true,
            protocol: P::TCP,
            pid: 1,
            index: 3,
            target: me,
            paris: None,
        };
        // SYN-ACK to 20483 acknowledging index 3, seq 9; MSS, SACK-perm.
        let mut seg = vec![0, 80, 0x50, 0x03, 0, 0, 0, 0, 0, 3, 0, 10, 0x70, 0x12, 0, 0];
        seg.extend([0; 4]);
        seg.extend([2, 4, 0x05, 0xb4, 4, 2, 1, 1]);
        let a = parse_tcp(&id, &seg, me, 64, 0).unwrap();
        assert_eq!(a.branch, Branch::V6Tcp);
        assert_eq!(a.seq, 10);
        assert_eq!(a.flags, "SA");
        assert_eq!(a.size, 8);
        assert_eq!(
            a.hdropts.as_deref(),
            Some(", \"hdropts\": [ { \"mss\":1460 }, { \"unknown-opt\":4 } ]")
        );
        let other = TraceId { index: 4, ..id };
        assert_eq!(parse_tcp(&other, &seg, me, 64, 0), None);
    }

    #[test]
    fn icmp_extensions_with_a_bad_checksum_say_nothing() {
        assert_eq!(
            icmp_multi(&[0x20, 0, 0, 1, 0, 8, 1, 1, 0, 0, 0, 0], false),
            None
        );
        assert_eq!(icmp_multi(&[0x20, 0, 0], false), None);
    }
}
