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
