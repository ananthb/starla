//! ICMP packet handling
//!
//! Provides utilities for constructing and parsing ICMPv4 and ICMPv6 packets.

use pnet::packet::icmp::{
    checksum, destination_unreachable::DestinationUnreachablePacket, echo_reply::EchoReplyPacket,
    echo_request::MutableEchoRequestPacket, time_exceeded::TimeExceededPacket, IcmpCode,
    IcmpPacket, IcmpTypes,
};
use pnet::packet::icmpv6::{
    echo_reply::EchoReplyPacket as EchoReplyPacketV6,
    echo_request::MutableEchoRequestPacket as MutableEchoRequestPacketV6, Icmpv6Code, Icmpv6Packet,
    Icmpv6Types,
};
use pnet::packet::Packet;
use std::io;

/// Construct an ICMPv4 Echo Request packet
pub fn build_icmpv4_echo_request(
    buffer: &mut [u8],
    identifier: u16,
    sequence: u16,
    payload: &[u8],
) -> io::Result<usize> {
    let mut packet = MutableEchoRequestPacket::new(buffer).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "Buffer too small for ICMPv4 packet",
        )
    })?;

    packet.set_icmp_type(IcmpTypes::EchoRequest);
    packet.set_icmp_code(IcmpCode::new(0));
    packet.set_identifier(identifier);
    packet.set_sequence_number(sequence);
    packet.set_payload(payload);

    // Calculate checksum on the immutable packet view
    let icmp_packet = IcmpPacket::new(packet.packet()).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "Failed to create ICMP packet for checksum",
        )
    })?;
    let checksum = checksum(&icmp_packet);
    packet.set_checksum(checksum);

    Ok(packet.packet().len())
}

/// Construct an ICMPv6 Echo Request packet
pub fn build_icmpv6_echo_request(
    buffer: &mut [u8],
    identifier: u16,
    sequence: u16,
    payload: &[u8],
) -> io::Result<usize> {
    let mut packet = MutableEchoRequestPacketV6::new(buffer).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "Buffer too small for ICMPv6 packet",
        )
    })?;

    packet.set_icmpv6_type(Icmpv6Types::EchoRequest);
    packet.set_icmpv6_code(Icmpv6Code::new(0));
    packet.set_identifier(identifier);
    packet.set_sequence_number(sequence);
    packet.set_payload(payload);

    // Note: ICMPv6 checksum is typically handled by the kernel for raw sockets.
    // If we were using raw IP sockets (not IPPROTO_ICMPV6), we would need to
    // calculate it including the pseudo-header.

    Ok(packet.packet().len())
}

/// Write the ICMPv6 checksum of `packet`, an ICMPv6 message, into it: the
/// one's complement sum over the message and the pseudo-header of the IPv6
/// packet that carries it from `src` to `dst` (RFC 4443 2.3, RFC 8200 8.1).
/// A unix kernel fills it in for its ICMPv6 sockets (RFC 3542 3.1); a
/// Windows raw socket is given it, as Microsoft's raw ping sample computes
/// it, so the requests leave with it whether the stack fills it in or not.
pub fn set_icmpv6_checksum(packet: &mut [u8], src: std::net::Ipv6Addr, dst: std::net::Ipv6Addr) {
    let Some(message) = Icmpv6Packet::new(packet) else {
        return;
    };
    // the checksum word itself is skipped
    let sum = pnet::packet::icmpv6::checksum(&message, &src, &dst);
    packet[2..4].copy_from_slice(&sum.to_be_bytes());
}

/// Result of parsing an ICMP response
#[derive(Debug, Clone, PartialEq)]
pub enum IcmpResponse {
    EchoReply { identifier: u16, sequence: u16 },
    TimeExceeded(Vec<u8>),           // Inner packet data
    DestinationUnreachable(Vec<u8>), // Inner packet data
    Other,
}

/// Parse an ICMPv4 packet
pub fn parse_icmpv4_packet(buffer: &[u8]) -> Option<IcmpResponse> {
    let packet = IcmpPacket::new(buffer)?;
    match packet.get_icmp_type() {
        IcmpTypes::EchoReply => {
            let reply = EchoReplyPacket::new(buffer)?;
            Some(IcmpResponse::EchoReply {
                identifier: reply.get_identifier(),
                sequence: reply.get_sequence_number(),
            })
        }
        IcmpTypes::TimeExceeded => {
            let te = TimeExceededPacket::new(buffer)?;
            Some(IcmpResponse::TimeExceeded(te.payload().to_vec()))
        }
        IcmpTypes::DestinationUnreachable => {
            let du = DestinationUnreachablePacket::new(buffer)?;
            Some(IcmpResponse::DestinationUnreachable(du.payload().to_vec()))
        }
        _ => Some(IcmpResponse::Other),
    }
}

/// Parse an ICMPv6 packet
pub fn parse_icmpv6_packet(buffer: &[u8]) -> Option<IcmpResponse> {
    let packet = Icmpv6Packet::new(buffer)?;
    match packet.get_icmpv6_type() {
        Icmpv6Types::EchoReply => {
            let reply = EchoReplyPacketV6::new(buffer)?;
            Some(IcmpResponse::EchoReply {
                identifier: reply.get_identifier(),
                sequence: reply.get_sequence_number(),
            })
        }
        // ICMPv6 error messages have different type codes
        // Type 1: Destination Unreachable
        // Type 3: Time Exceeded
        _ if packet.get_icmpv6_type().0 == 1 => {
            // Destination Unreachable - payload is after 4 byte header
            Some(IcmpResponse::DestinationUnreachable(
                buffer.get(8..)?.to_vec(),
            ))
        }
        _ if packet.get_icmpv6_type().0 == 3 => {
            // Time Exceeded - payload is after 4 byte header
            Some(IcmpResponse::TimeExceeded(buffer.get(8..)?.to_vec()))
        }
        _ => Some(IcmpResponse::Other),
    }
}

/// Parse an ICMPv4 packet to check if it is an Echo Reply (legacy helper)
pub fn parse_icmpv4_echo_reply(buffer: &[u8]) -> Option<(u16, u16)> {
    match parse_icmpv4_packet(buffer) {
        Some(IcmpResponse::EchoReply {
            identifier,
            sequence,
        }) => Some((identifier, sequence)),
        _ => None,
    }
}

/// Parse an ICMPv6 packet to check if it is an Echo Reply (legacy helper)
pub fn parse_icmpv6_echo_reply(buffer: &[u8]) -> Option<(u16, u16)> {
    match parse_icmpv6_packet(buffer) {
        Some(IcmpResponse::EchoReply {
            identifier,
            sequence,
        }) => Some((identifier, sequence)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_icmpv4_build_parse() {
        let mut buffer = [0u8; 64];
        let payload = b"hello";
        let size = build_icmpv4_echo_request(&mut buffer, 1234, 1, payload).unwrap();

        let packet = IcmpPacket::new(&buffer[..size]).unwrap();
        assert_eq!(packet.get_icmp_type(), IcmpTypes::EchoRequest);

        // Simulate Echo Reply
        buffer[0] = 0; // Echo Reply type

        let parsed = parse_icmpv4_packet(&buffer[..size]);
        assert_eq!(
            parsed,
            Some(IcmpResponse::EchoReply {
                identifier: 1234,
                sequence: 1
            })
        );
    }

    /// The sum over the pseudo-header and the message, checksum included,
    /// is all ones: the receiver's check (RFC 1071).
    #[test]
    fn icmpv6_checksums_verify() {
        use std::net::Ipv6Addr;
        let src: Ipv6Addr = "2001:db8::31".parse().unwrap();
        let dst: Ipv6Addr = "2001:7fd::1".parse().unwrap();
        let mut packet = [0u8; 40];
        let len = build_icmpv6_echo_request(&mut packet, 0x1234, 7, &[0xa5; 32]).unwrap();
        let packet = &mut packet[..len];
        packet[2..4].copy_from_slice(&[0xde, 0xad]); // stale: overwritten
        set_icmpv6_checksum(packet, src, dst);
        let mut words: Vec<u8> = Vec::new();
        words.extend_from_slice(&src.octets());
        words.extend_from_slice(&dst.octets());
        words.extend_from_slice(&(len as u32).to_be_bytes());
        words.extend_from_slice(&[0, 0, 0, 58]);
        words.extend_from_slice(packet);
        let mut sum: u32 = words
            .chunks(2)
            .map(|w| u32::from(u16::from_be_bytes([w[0], *w.get(1).unwrap_or(&0)])))
            .sum();
        while sum > 0xffff {
            sum = (sum & 0xffff) + (sum >> 16);
        }
        assert_eq!(sum, 0xffff);
        // another source: another checksum
        let mut other = packet.to_vec();
        set_icmpv6_checksum(&mut other, "2001:db8::32".parse().unwrap(), dst);
        assert_ne!(other[2..4], packet[2..4]);
        // too short to be a message: left alone
        let mut short = [1u8, 2, 3];
        set_icmpv6_checksum(&mut short, src, dst);
        assert_eq!(short, [1, 2, 3]);
    }

    #[test]
    fn test_icmp_error_parsing() {
        let mut buffer = [0u8; 64];

        // Time Exceeded
        buffer[0] = 11; // Time Exceeded type
                        // The packet parser expects enough data for headers.
                        // MutablePacket::new verifies minimum size.
                        // TimeExceededPacket min size is 4 bytes + payload.
        let parsed = parse_icmpv4_packet(&buffer[..8]);
        // We expect TimeExceeded with empty payload since buffer is zeros
        assert!(matches!(parsed, Some(IcmpResponse::TimeExceeded(_))));

        // Destination Unreachable
        buffer[0] = 3; // Destination Unreachable type
        let parsed = parse_icmpv4_packet(&buffer[..8]);
        assert!(matches!(
            parsed,
            Some(IcmpResponse::DestinationUnreachable(_))
        ));
    }
}
