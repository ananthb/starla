//! Reading the server's answer as sslgetcert does
//! (probe-busybox/eperd/sslgetcert.c, readcb() and the eat_* functions,
//! 1110-1616).
//!
//! sslgetcert parses the TLS 1.2 plaintext handshake itself: it takes the
//! records off the stream, gathers their payloads in a message buffer,
//! reads the cipher suite from the ServerHello and every certificate from
//! the Certificate message, and validates nothing — the certificate, self-
//! signed or not, is the measurement. This is a byte-for-byte port of that
//! reader, fed with whatever the socket returns, so a split record, an
//! alert, a short message or a stray record type ends where the
//! reference's ends.

use super::hello::{MSG_ALERT, MSG_HANDSHAKE};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;

/// sslgetcert.c:140-141
const HS_SERVER_HELLO: u8 = 2;
const HS_CERTIFICATE: u8 = 11;

/// What one readcb() run comes to.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// The reader needs more bytes: readcb() returned and waits for the next
    /// read event (and the timer keeps running).
    Wait,
    /// An alert came where the ServerHello was expected (eat_alert()):
    /// reported with report_head(), then done.
    Alert { level: u8, description: u8 },
    /// The Certificate message, complete and consistent (eat_certificate()):
    /// reported with report_head(), then done.
    Certs(Vec<Vec<u8>>),
    /// report() with `"err":"<text>"`, then done.
    Err(&'static str),
}

/// sslgetcert.c:42, `enum readstate`
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ReadState {
    Hello,
    Certs,
    Done,
}

/// The reader's state across read events: the stream bytes not yet framed
/// (state->inbuf), the handshake payload of the records framed so far
/// (state->msginbuf.buffer), and what report_head() prints.
#[derive(Debug)]
pub struct Reader {
    /// Received from the socket and not yet taken by buf_read() (what sits
    /// in the bufferevent's input buffer)
    pending: Vec<u8>,
    inbuf: Vec<u8>,
    in_off: usize,
    msg: Vec<u8>,
    msg_off: usize,
    /// The version bytes of the last record header seen (msgbuf_read()
    /// writes state->recv_major/minor): what "method" and "ver" report —
    /// the record layer's version, not the ServerHello's.
    pub recv_major: u8,
    pub recv_minor: u8,
    /// The ServerHello's cipher suite, 0 until one is read
    pub server_cipher: u16,
    pub state: ReadState,
}

impl Default for Reader {
    fn default() -> Self {
        Self::new()
    }
}

/// Cap on bytes buffered from a hostile or broken server: the socket bytes
/// not yet framed, and the handshake payload framed so far. The reference has
/// no cap; without one a fast server can force unbounded allocation during the
/// measurement's timeout window.
const MAX_PENDING: usize = 1 << 20;
const MAX_MSG: usize = 1 << 20;

impl Reader {
    pub fn new() -> Self {
        Self {
            pending: Vec::new(),
            inbuf: Vec::new(),
            in_off: 0,
            msg: Vec::new(),
            msg_off: 0,
            recv_major: 0,
            recv_minor: 0,
            server_cipher: 0,
            state: ReadState::Hello,
        }
    }

    /// Bytes the socket returned.
    pub fn feed(&mut self, data: &[u8]) {
        let room = MAX_PENDING.saturating_sub(self.pending.len());
        self.pending
            .extend_from_slice(&data[..data.len().min(room)]);
    }

    /// readcb(), sslgetcert.c:1110-1155
    pub fn readcb(&mut self) -> Event {
        loop {
            match self.state {
                ReadState::Hello => match self.eat_server_hello() {
                    None => {
                        self.state = ReadState::Certs;
                        continue;
                    }
                    Some(Event::Wait) => return Event::Wait,
                    Some(ev) => {
                        self.state = ReadState::Done;
                        return ev;
                    }
                },
                ReadState::Certs => {
                    let ev = self.eat_certificate();
                    if ev != Event::Wait {
                        self.state = ReadState::Done;
                    }
                    return ev;
                }
                ReadState::Done => return Event::Wait,
            }
        }
    }

    /// buf_read(), sslgetcert.c:259-340: take what the bufferevent holds,
    /// or fail with EAGAIN when it holds nothing.
    fn buf_read(&mut self) -> bool {
        if self.pending.is_empty() {
            return false;
        }
        self.inbuf.drain(..self.in_off);
        self.in_off = 0;
        self.inbuf.append(&mut self.pending);
        true
    }

    /// msgbuf_read(), sslgetcert.c:384-437: append the next record's payload
    /// to the message buffer and return its type; None is EAGAIN.
    fn msgbuf_read(&mut self) -> Option<u8> {
        loop {
            let avail = self.inbuf.len() - self.in_off;
            if avail < 5 {
                if !self.buf_read() {
                    return None;
                }
                continue;
            }
            let p = &self.inbuf[self.in_off..];
            let typ = p[0];
            // Written before the length check, as the reference does
            self.recv_major = p[1];
            self.recv_minor = p[2];
            let len = u16::from_be_bytes([p[3], p[4]]) as usize;
            if avail < 5 + len {
                if !self.buf_read() {
                    return None;
                }
                continue;
            }
            let start = self.in_off + 5;
            // Drop already-consumed payload, then refuse to grow the message
            // buffer past the cap (the caller's timeout reports the failure).
            if self.msg_off == self.msg.len() {
                self.msg.clear();
                self.msg_off = 0;
            }
            if self.msg.len() + len > MAX_MSG {
                return None;
            }
            self.msg.extend_from_slice(&self.inbuf[start..start + len]);
            self.in_off += 5 + len;
            return Some(typ);
        }
    }

    fn msg_avail(&self) -> usize {
        self.msg.len() - self.msg_off
    }

    /// eat_alert(), sslgetcert.c:1264-1320
    fn eat_alert(&mut self) -> Event {
        loop {
            if self.msg_avail() < 2 {
                match self.msgbuf_read() {
                    None => return Event::Wait,
                    Some(MSG_ALERT) => continue,
                    // "got bad type": -1 without a report, so the reference
                    // waits for more data (and its timer)
                    Some(_) => return Event::Wait,
                }
            }
            let level = self.msg[self.msg_off];
            let description = self.msg[self.msg_off + 1];
            self.msg_off += 2;
            return Event::Alert { level, description };
        }
    }

    /// eat_server_hello(), sslgetcert.c:1322-1459. None: the ServerHello was
    /// read; otherwise what readcb() returns.
    fn eat_server_hello(&mut self) -> Option<Event> {
        loop {
            if self.msg_avail() < 4 {
                match self.msgbuf_read() {
                    None => return Some(Event::Wait),
                    Some(MSG_ALERT) => return Some(self.eat_alert()),
                    Some(MSG_HANDSHAKE) => continue,
                    Some(_) => return Some(Event::Err("bad type")),
                }
            }
            let p = &self.msg[self.msg_off..];
            if p[0] != HS_SERVER_HELLO {
                return Some(Event::Err("bad type"));
            }
            let len = u24(p[1], p[2], p[3]);
            if p.len() < 4 + len {
                match self.msgbuf_read() {
                    None => return Some(Event::Wait),
                    Some(MSG_HANDSHAKE) => continue,
                    Some(_) => return Some(Event::Err("bad type")),
                }
            }

            let totlen = 4 + len;
            let mut o = 4;
            if o + 2 > totlen {
                return Some(Event::Err("Server Hello too short (ProtocolVersion)"));
            }
            o += 2;
            if o + 32 > totlen {
                return Some(Event::Err("Server Hello too short (Random)"));
            }
            o += 32;
            if o + 1 > totlen {
                return Some(Event::Err("Server Hello too short (SessionID len)"));
            }
            let seslen = p[o] as usize;
            o += 1;
            if seslen > 32 {
                return Some(Event::Err("Server Hello bad SessionID len"));
            }
            if o + seslen > totlen {
                return Some(Event::Err("Server Hello too short (SessionID)"));
            }
            o += seslen;
            if o + 2 > totlen {
                return Some(Event::Err("Server Hello too short (CipherSuite)"));
            }
            self.server_cipher = u16::from_be_bytes([p[o], p[o + 1]]);
            // Compression and extensions are skipped with the rest
            self.msg_off += totlen;
            return None;
        }
    }

    /// eat_certificate(), sslgetcert.c:1461-1616
    fn eat_certificate(&mut self) -> Event {
        loop {
            if self.msg_avail() < 4 {
                match self.msgbuf_read() {
                    None => return Event::Wait,
                    Some(MSG_HANDSHAKE) => continue,
                    Some(_) => return Event::Err("bad type"),
                }
            }
            let p = &self.msg[self.msg_off..];
            if p[0] != HS_CERTIFICATE {
                return Event::Err("bad type");
            }
            let len = u24(p[1], p[2], p[3]);
            if p.len() < 4 + len {
                match self.msgbuf_read() {
                    None => return Event::Wait,
                    Some(MSG_HANDSHAKE) => continue,
                    Some(_) => return Event::Err("bad type"),
                }
            }

            // The reference reads the list length without checking len >= 3;
            // past the message it reads whatever its buffer holds there.
            let base = self.msg_off + 4;
            let at = |i: usize| self.msg.get(base + i).copied().unwrap_or(0);
            let n = u24(at(0), at(1), at(2));
            let mut o = 3;
            let mut certs = Vec::new();
            while o < 3 + n {
                // Past the message the reference reads a garbage length, but
                // any length then fails its o+3+slen > len.
                if o + 3 > len {
                    return Event::Err("bad len");
                }
                let slen = u24(at(o), at(o + 1), at(o + 2));
                if o + 3 + slen > len {
                    return Event::Err("bad len");
                }
                certs.push(self.msg[base + o + 3..base + o + 3 + slen].to_vec());
                o += 3 + slen;
            }
            if o != 3 + n || o != len {
                return Event::Err("bad amount of cert data");
            }
            self.msg_off += 4 + len;
            return Event::Certs(certs);
        }
    }
}

fn u24(a: u8, b: u8, c: u8) -> usize {
    ((a as usize) << 16) | ((b as usize) << 8) | c as usize
}

/// One "cert" array element as eat_certificate() prints it
/// (sslgetcert.c:1562-1581): buf_add_b64() breaks the base64 after every 48
/// input bytes, the newlines are written as `\n` escapes, and the last line
/// always ends with exactly one. An empty certificate is BEGIN, `\n`, END.
pub fn pem_json(der: &[u8]) -> String {
    let b64 = BASE64.encode(der);
    let mut out = String::with_capacity(b64.len() + b64.len() / 32 + 64);
    out.push_str("\"-----BEGIN CERTIFICATE-----\\n");
    for line in b64.as_bytes().chunks(64) {
        // base64 output is ASCII
        out.push_str(std::str::from_utf8(line).unwrap_or_default());
        out.push_str("\\n");
    }
    out.push_str("-----END CERTIFICATE-----\"");
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// The server's first flight to the reference probe in the twin run of
    /// 2026-10-13 (probe-ref.pcap, port 43042, 597 bytes): one record,
    /// version 0303, holding ServerHello (c02b, a TLS 1.3 server's DOWNGRD
    /// random), Certificate (one self-signed P-256 certificate),
    /// ServerKeyExchange and ServerHelloDone.
    pub(crate) const REF_SERVER_FLIGHT: &str = "16030302500200004c03037a4d3cff5baab8069618cbeb24e31ba6c46808fd7923d352444f574e47524401203d65c1626b9d0d192cba0e5cf33cc9b008ebe9f1e2b044079fdf1a22a40c5875c02b000004000000000b00018500018200017f3082017b30820122a0030201020214246ce72b5fda12a8388bc05e2bf1ac47b3f9b674300a06082a8648ce3d0403023021311f301d06035504030c16726367656e2073656c66207369676e656420636572743020170d3735303130313030303030305a180f34303936303130313030303030305a3021311f301d06035504030c16726367656e2073656c66207369676e656420636572743059301306072a8648ce3d020106082a8648ce3d03010703420004f8e7f4e54d30bb28faa214dab1725aec7cc91ff6927d13fb7dcc0c1cc9c3a73d5121a82a2a25792803f1c40811c1e6756659d2fac74a541c6b3293f9acf171dea336303430320603551d11042b3029820c7777772e726970652e6e65748208726970652e6e6574820f61746c61732d7477696e2e74657374300a06082a8648ce3d040302034700304402200320ef7ddaca5024eed647465662308c071f80252ca8eb95854a0e2569878579022002777c8f8228c6a936eadcd461d6505f731042bcb3ef881c33eb07f67e449c040c00006f03001d2089c8e70e65742fdd4abcba86fb5484f7bf45470b4b6b090af18b6606c01ea13c04030047304502205adabec6c458cd98e62b646f28041b7ae76b13b9a1dcff4018cd6e97b61a4bf7022100ed9e056f97f939c55f839a435544f9c60684ceb737840847b29e433b2d0fcb5b0e000000";

    /// The "cert" element of the reference's RESULT for 15001 from that
    /// flight, as uploaded.
    pub(crate) const REF_CERT_JSON: &str = r#""-----BEGIN CERTIFICATE-----\nMIIBezCCASKgAwIBAgIUJGznK1/aEqg4i8BeK/GsR7P5tnQwCgYIKoZIzj0EAwIw\nITEfMB0GA1UEAwwWcmNnZW4gc2VsZiBzaWduZWQgY2VydDAgFw03NTAxMDEwMDAw\nMDBaGA80MDk2MDEwMTAwMDAwMFowITEfMB0GA1UEAwwWcmNnZW4gc2VsZiBzaWdu\nZWQgY2VydDBZMBMGByqGSM49AgEGCCqGSM49AwEHA0IABPjn9OVNMLso+qIU2rFy\nWux8yR/2kn0T+33MDBzJw6c9USGoKioleSgD8cQIEcHmdWZZ0vrHSlQcazKT+azx\ncd6jNjA0MDIGA1UdEQQrMCmCDHd3dy5yaXBlLm5ldIIIcmlwZS5uZXSCD2F0bGFz\nLXR3aW4udGVzdDAKBggqhkjOPQQDAgNHADBEAiADIO992spQJO7WR0ZWYjCMBx+A\nJSyo65WFSg4laYeFeQIgAnd8j4Ioxqk26tzUYdZQX3MQQryz74gcM+sH9n5EnAQ=\n-----END CERTIFICATE-----""#;

    pub(crate) fn ref_flight() -> Vec<u8> {
        hex::decode(REF_SERVER_FLIGHT).unwrap()
    }

    /// The certificate inside that flight
    pub(crate) fn ref_cert_der() -> Vec<u8> {
        let flight = ref_flight();
        // record 5 + ServerHello 4+76 + Certificate header 4 + list 3 + len 3
        flight[5 + 80 + 10..5 + 80 + 10 + 383].to_vec()
    }

    #[test]
    fn the_twin_flight_reads_as_the_reference_read_it() {
        let mut r = Reader::new();
        r.feed(&ref_flight());
        let ev = r.readcb();
        assert_eq!(ev, Event::Certs(vec![ref_cert_der()]));
        assert_eq!((r.recv_major, r.recv_minor), (3, 3));
        assert_eq!(r.server_cipher, 0xc02b);
        assert_eq!(pem_json(&ref_cert_der()), REF_CERT_JSON);
    }

    /// The reference keeps its buffers across read events: the same flight
    /// a byte at a time ends the same way.
    #[test]
    fn a_flight_split_anywhere_reads_the_same() {
        let flight = ref_flight();
        let mut r = Reader::new();
        let mut last = Event::Wait;
        for (i, b) in flight.iter().enumerate() {
            r.feed(&[*b]);
            last = r.readcb();
            if last != Event::Wait {
                // msgbuf_read() frames whole records only, and this one
                // carries the whole flight
                assert_eq!(i, flight.len() - 1);
                break;
            }
        }
        assert_eq!(last, Event::Certs(vec![ref_cert_der()]));
    }

    /// A Certificate message spread over two records is gathered from both,
    /// and "ver" is then the last record's version.
    #[test]
    fn a_certificate_across_records_is_gathered() {
        let flight = ref_flight();
        let hs = &flight[5..]; // the record's payload
        let (a, b) = hs.split_at(200);
        let mut stream = vec![0x16, 0x03, 0x03];
        stream.extend_from_slice(&(a.len() as u16).to_be_bytes());
        stream.extend_from_slice(a);
        stream.extend_from_slice(&[0x16, 0x03, 0x01]);
        stream.extend_from_slice(&(b.len() as u16).to_be_bytes());
        stream.extend_from_slice(b);
        let mut r = Reader::new();
        r.feed(&stream);
        assert_eq!(r.readcb(), Event::Certs(vec![ref_cert_der()]));
        assert_eq!((r.recv_major, r.recv_minor), (3, 1));
    }

    /// What a TLS 1.3-only server answers a hello without supported_versions:
    /// a protocol_version alert, reported with cipher 0.
    #[test]
    fn an_alert_instead_of_a_server_hello() {
        let mut r = Reader::new();
        r.feed(&[0x15, 0x03, 0x03, 0x00, 0x02, 0x02, 0x46]);
        assert_eq!(
            r.readcb(),
            Event::Alert {
                level: 2,
                description: 70
            }
        );
        assert_eq!(r.server_cipher, 0);
        assert_eq!(r.state, ReadState::Done);
    }

    #[test]
    fn a_record_that_is_neither_handshake_nor_alert_is_bad_type() {
        let mut r = Reader::new();
        // application_data
        r.feed(&[0x17, 0x03, 0x03, 0x00, 0x04, 1, 2, 3, 4]);
        assert_eq!(r.readcb(), Event::Err("bad type"));
        // a handshake message that is not a ServerHello
        let mut r = Reader::new();
        r.feed(&[0x16, 0x03, 0x03, 0x00, 0x04, 0x0b, 0, 0, 0]);
        assert_eq!(r.readcb(), Event::Err("bad type"));
    }

    /// An alert after the ServerHello is not eat_alert()'s: "bad type".
    #[test]
    fn an_alert_after_the_server_hello_is_bad_type() {
        let flight = ref_flight();
        let hello = &flight[5..5 + 80];
        let mut stream = vec![0x16, 0x03, 0x03, 0x00, 80];
        stream.extend_from_slice(hello);
        stream.extend_from_slice(&[0x15, 0x03, 0x03, 0x00, 0x02, 0x02, 0x28]);
        let mut r = Reader::new();
        r.feed(&stream);
        assert_eq!(r.readcb(), Event::Err("bad type"));
        assert_eq!(r.server_cipher, 0xc02b);
    }

    #[test]
    fn a_short_server_hello_says_which_field() {
        // ServerHello with len 2: version only
        let mut r = Reader::new();
        r.feed(&[0x16, 0x03, 0x03, 0x00, 0x06, 0x02, 0, 0, 2, 3, 3]);
        assert_eq!(r.readcb(), Event::Err("Server Hello too short (Random)"));
        // session id length 33
        let mut body = vec![3, 3];
        body.extend_from_slice(&[0u8; 32]);
        body.push(33);
        let mut msg = vec![0x02, 0, 0, body.len() as u8];
        msg.extend_from_slice(&body);
        let mut rec = vec![0x16, 0x03, 0x03, 0, msg.len() as u8];
        rec.extend_from_slice(&msg);
        let mut r = Reader::new();
        r.feed(&rec);
        assert_eq!(r.readcb(), Event::Err("Server Hello bad SessionID len"));
    }

    #[test]
    fn inconsistent_certificate_lengths() {
        let flight = ref_flight();
        let hello = flight[5..5 + 80].to_vec();
        let with_cert = |cert_msg: &[u8]| {
            let mut payload = hello.clone();
            payload.extend_from_slice(cert_msg);
            let mut rec = vec![0x16, 0x03, 0x03];
            rec.extend_from_slice(&(payload.len() as u16).to_be_bytes());
            rec.extend_from_slice(&payload);
            let mut r = Reader::new();
            r.feed(&rec);
            r.readcb()
        };
        // one 2-byte certificate claiming 3 bytes
        let bad_len = [0x0b, 0, 0, 8, 0, 0, 5, 0, 0, 3, 0xaa, 0xbb];
        assert_eq!(with_cert(&bad_len[..]), Event::Err("bad len"));
        // list length 0 in a 5-byte message
        let bad_amount = [0x0b, 0, 0, 5, 0, 0, 0, 0xaa, 0xbb];
        assert_eq!(
            with_cert(&bad_amount[..]),
            Event::Err("bad amount of cert data")
        );
        // an empty list is a valid, empty "cert" array
        assert_eq!(
            with_cert(&[0x0b, 0, 0, 3, 0, 0, 0][..]),
            Event::Certs(vec![])
        );
    }

    /// buf_add_b64() and the escaping loop, literally (sslgetcert.c:209-257,
    /// 1562-1581).
    fn reference_pem_json(data: &[u8]) -> String {
        const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut buf = Vec::new();
        let mut i = 0;
        while i + 3 <= data.len() {
            let v = ((data[i] as u32) << 16) + ((data[i + 1] as u32) << 8) + data[i + 2] as u32;
            for s in [18, 12, 6, 0] {
                buf.push(B64[((v >> s) & 63) as usize]);
            }
            if i % 48 == 45 {
                buf.push(b'\n');
            }
            i += 3;
        }
        match data.len() - i {
            1 => {
                let v = (data[i] as u32) << 16;
                buf.extend_from_slice(&[
                    B64[(v >> 18) as usize & 63],
                    B64[(v >> 12) as usize & 63],
                ]);
                buf.extend_from_slice(b"==");
            }
            2 => {
                let v = ((data[i] as u32) << 16) + ((data[i + 1] as u32) << 8);
                for s in [18, 12, 6] {
                    buf.push(B64[((v >> s) & 63) as usize]);
                }
                buf.push(b'=');
            }
            _ => {}
        }
        let mut out = String::from("\"-----BEGIN CERTIFICATE-----\\n");
        let mut need_nl = false;
        for c in buf {
            if c == b'\n' {
                out.push_str("\\n");
                need_nl = false;
            } else {
                out.push(c as char);
                need_nl = true;
            }
        }
        if need_nl {
            out.push_str("\\n");
        }
        out.push_str("-----END CERTIFICATE-----\"");
        out
    }

    #[test]
    fn pem_lines_break_where_buf_add_b64_breaks_them() {
        let data: Vec<u8> = (0..=255u8).cycle().take(1000).collect();
        for len in [0, 1, 2, 3, 45, 46, 47, 48, 49, 50, 95, 96, 97, 383, 1000] {
            assert_eq!(
                pem_json(&data[..len]),
                reference_pem_json(&data[..len]),
                "len {len}"
            );
        }
    }
}
