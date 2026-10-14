//! Winsock's ancillary data, as WSARecvMsg hands it over in WSAMSG's
//! Control buffer: a run of control messages, each a WSACMSGHDR
//! (ws2def.h) — `cmsg_len`, a SIZE_T that counts the header, then the
//! INTs `cmsg_level` and `cmsg_type` — and its data at
//! WSA_CMSGDATA_ALIGN(sizeof(WSACMSGHDR)), the next message at
//! WSA_CMSGHDR_ALIGN(cmsg_len). Both alignments are the SIZE_T's
//! (TYPE_ALIGNMENT(WSACMSGHDR), MAX_NATURAL_ALIGNMENT): on the 64-bit
//! Windows Starla ships for, a 16-byte header and 8-byte steps.
//!
//! It is plain bytes, so the parse builds and is tested everywhere; the
//! Windows ping reads its replies' hop limit and destination with it
//! (`windows_icmp::RawIcmp6Socket`), as ping.c reads them from recvmsg's
//! (ping.c:1150-1162).

use crate::RecvMeta;
use std::mem::size_of;
use std::net::Ipv6Addr;

/// IPPROTO_IPV6 (ws2def.h)
pub const IPPROTO_IPV6: i32 = 41;
/// IPV6_PKTINFO (ws2ipdef.h): set to 1, WSARecvMsg gives an IN6_PKTINFO,
/// the address the packet came to and then the interface index.
pub const IPV6_PKTINFO: i32 = 19;
/// IPV6_HOPLIMIT (ws2ipdef.h): set to 1, WSARecvMsg gives the packet's hop
/// limit, an INT. Windows has no IPV6_RECVHOPLIMIT; this is the option
/// that asks for it.
pub const IPV6_HOPLIMIT: i32 = 21;

/// The SIZE_T's alignment: every header and every data part starts on it.
const ALIGN: usize = size_of::<usize>();
/// sizeof(WSACMSGHDR)
const HEADER: usize = size_of::<usize>() + 2 * size_of::<i32>();
/// Where a message's data starts: WSA_CMSG_DATA
const DATA: usize = align(HEADER);

const fn align(len: usize) -> usize {
    (len + ALIGN - 1) & !(ALIGN - 1)
}

/// One control message as Winsock lays it out, WSA_CMSG_SPACE(`data`)
/// bytes long with `cmsg_len` WSA_CMSG_LEN(`data`): what WSARecvMsg writes
/// for it, and what the tests give [`recv_meta`].
pub fn cmsg(level: i32, kind: i32, data: &[u8]) -> Vec<u8> {
    let len = DATA + data.len();
    let mut out = vec![0u8; align(len)];
    out[..ALIGN].copy_from_slice(&len.to_ne_bytes());
    out[ALIGN..ALIGN + 4].copy_from_slice(&level.to_ne_bytes());
    out[ALIGN + 4..HEADER].copy_from_slice(&kind.to_ne_bytes());
    out[DATA..len].copy_from_slice(data);
    out
}

/// The hop limit (IPV6_HOPLIMIT) and the destination address
/// (IPV6_PKTINFO) among WSARecvMsg's control messages, as `recvmsg_meta`
/// reads them on unix; anything else is passed over. `control` is the
/// Control buffer up to the length WSARecvMsg left in Control.len. A
/// message that claims more than is there ends the walk, as
/// WSA_CMSG_NXTHDR ends it.
pub fn recv_meta(control: &[u8]) -> RecvMeta {
    let mut meta = RecvMeta::default();
    let mut at = 0;
    while let Some(header) = control.get(at..).and_then(|rest| rest.get(..HEADER)) {
        let len = usize::from_ne_bytes(header[..ALIGN].try_into().unwrap_or_default());
        let level = i32::from_ne_bytes(header[ALIGN..ALIGN + 4].try_into().unwrap_or_default());
        let kind = i32::from_ne_bytes(header[ALIGN + 4..].try_into().unwrap_or_default());
        let Some(data) = len
            .checked_sub(DATA)
            .and_then(|n| control.get(at + DATA..)?.get(..n))
        else {
            break;
        };
        match (level, kind) {
            (IPPROTO_IPV6, IPV6_HOPLIMIT) => {
                if let Some(int) = data.get(..4) {
                    let hops = i32::from_ne_bytes(int.try_into().unwrap_or_default());
                    meta.ttl = u8::try_from(hops).ok();
                }
            }
            (IPPROTO_IPV6, IPV6_PKTINFO) => {
                if let Some(addr) = data.get(..16) {
                    let addr: [u8; 16] = addr.try_into().unwrap_or_default();
                    meta.dst = Some(Ipv6Addr::from(addr).into());
                }
            }
            _ => {}
        }
        at += align(len);
    }
    meta
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pktinfo(addr: &str, ifindex: u32) -> Vec<u8> {
        let mut data = addr.parse::<Ipv6Addr>().unwrap().octets().to_vec();
        data.extend_from_slice(&ifindex.to_ne_bytes());
        cmsg(IPPROTO_IPV6, IPV6_PKTINFO, &data)
    }

    fn hoplimit(hops: i32) -> Vec<u8> {
        cmsg(IPPROTO_IPV6, IPV6_HOPLIMIT, &hops.to_ne_bytes())
    }

    /// ws2def.h's layout on 64-bit Windows: WSA_CMSG_LEN(4) is 20 and
    /// WSA_CMSG_SPACE(4) 24; an IN6_PKTINFO (20 bytes) takes 40.
    #[cfg(target_pointer_width = "64")]
    #[test]
    fn messages_are_laid_out_as_ws2def_lays_them_out() {
        let m = hoplimit(57);
        assert_eq!(m.len(), 24);
        assert_eq!(&m[..8], &20usize.to_ne_bytes());
        assert_eq!(&m[8..12], &41i32.to_ne_bytes());
        assert_eq!(&m[12..16], &21i32.to_ne_bytes());
        assert_eq!(&m[16..20], &57i32.to_ne_bytes());
        assert_eq!(pktinfo("2001:db8::31", 7).len(), 40);
    }

    /// Both, in either order, among others: the hop limit and the address
    /// the packet came to.
    #[test]
    fn the_hop_limit_and_destination_are_read() {
        let want = RecvMeta {
            ttl: Some(57),
            dst: Some("2001:418:1::33".parse().unwrap()),
        };
        let mut control = pktinfo("2001:418:1::33", 12);
        control.extend(hoplimit(57));
        assert_eq!(recv_meta(&control), want);
        let mut control = cmsg(0, 21, &[9; 4]); // IPPROTO_IP's 21: not it
        control.extend(hoplimit(57));
        control.extend(cmsg(IPPROTO_IPV6, 39, &[0; 4])); // IPV6_TCLASS
        control.extend(pktinfo("2001:418:1::33", 12));
        assert_eq!(recv_meta(&control), want);
        // a hop limit of 255 is one; nothing at all is none
        assert_eq!(recv_meta(&hoplimit(255)).ttl, Some(255));
        assert_eq!(recv_meta(&[]), RecvMeta::default());
    }

    /// A message cut short, or claiming less than its header, ends the
    /// walk; what came before it stands.
    #[test]
    fn a_short_or_bad_message_ends_the_walk() {
        let mut control = hoplimit(57);
        let pkt = pktinfo("2001:418:1::33", 12);
        control.extend_from_slice(&pkt[..pkt.len() - 8]);
        assert_eq!(
            recv_meta(&control),
            RecvMeta {
                ttl: Some(57),
                dst: None
            }
        );
        let mut bad = hoplimit(57);
        bad[..ALIGN].copy_from_slice(&3usize.to_ne_bytes());
        assert_eq!(recv_meta(&bad), RecvMeta::default());
        let mut huge = hoplimit(57);
        huge[..ALIGN].copy_from_slice(&usize::MAX.to_ne_bytes());
        assert_eq!(recv_meta(&huge), RecvMeta::default());
        assert_eq!(recv_meta(&hoplimit(57)[..HEADER - 1]), RecvMeta::default());
    }
}
