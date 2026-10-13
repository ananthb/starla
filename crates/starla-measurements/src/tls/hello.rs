//! The ClientHello sslgetcert writes (probe-busybox/eperd/sslgetcert.c
//! writecb(), 1632-1665).
//!
//! sslgetcert is not a TLS client: it writes one hand-built ClientHello,
//! reads the ServerHello and Certificate messages and hangs up. Starla used
//! to go through rustls instead, which put a different ClientHello on the
//! wire (record version 0301, TLS 1.3 offered, 9 suites, 10 extensions in a
//! random order, X25519MLKEM768) and validated the certificate. These are
//! the reference's bytes.

/// Record type of a handshake message (sslgetcert.c:138, MSG_HANDSHAKE)
pub const MSG_HANDSHAKE: u8 = 22;
/// Record type of an alert (sslgetcert.c:137, MSG_ALERT)
pub const MSG_ALERT: u8 = 21;
/// Handshake type of a ClientHello (sslgetcert.c:139)
const HS_CLIENT_HELLO: u8 = 1;

/// add_ciphers(), sslgetcert.c:573-593: Firefox 89.0.2's list, TLS 1.3
/// suites included although no supported_versions extension offers 1.3.
pub const CIPHERS: [u16; 18] = [
    0x1301, 0x1303, 0x1302, 0xc02b, 0xc02f, 0xcca9, 0xcca8, 0xc02c, 0xc030, 0xc00a, 0xc009, 0xc013,
    0xc014, 0x009c, 0x009d, 0x002f, 0x0035, 0x000a,
];

/// ext_sigs(), sslgetcert.c:618-632
pub const SIG_ALGS: [u16; 11] = [
    0x0403, 0x0503, 0x0603, 0x0804, 0x0805, 0x0806, 0x0401, 0x0501, 0x0601, 0x0203, 0x0201,
];

/// elliptic_curves(), sslgetcert.c:646-655
pub const GROUPS: [u16; 6] = [0x001d, 0x0017, 0x0018, 0x0019, 0x0100, 0x0101];

/// add_compression(), sslgetcert.c:605-612: a length of 2, then the methods
/// 1 (DEFLATE) and 0 (null).
const COMPRESSION: [u8; 3] = [2, 1, 0];

/// `-V`: the version the ClientHello asks for (sslgetcert.c:928-952). The
/// controller never sends -V, so this is always TLS 1.2.
pub const TLS_1_2: (u8, u8) = (3, 3);

fn put_u16(buf: &mut Vec<u8>, v: usize) {
    buf.extend_from_slice(&(v as u16).to_be_bytes());
}

/// add_random(), sslgetcert.c:539-560: time(NULL) big-endian, then 28 bytes
/// from /dev/urandom.
pub fn hello_random(now: u32) -> [u8; 32] {
    let mut random: [u8; 32] = rand::random();
    random[..4].copy_from_slice(&now.to_be_bytes());
    random
}

/// add_extensions(), sslgetcert.c:686-716: server_name (only with `-h`),
/// signature_algorithms, supported_groups — in that order, and nothing else.
fn extensions(sni: Option<&str>) -> Vec<u8> {
    let mut ext = Vec::new();
    if let Some(name) = sni {
        // sni(), sslgetcert.c:666-684
        put_u16(&mut ext, 0); // server_name
        put_u16(&mut ext, 2 + 1 + 2 + name.len());
        put_u16(&mut ext, 1 + 2 + name.len());
        ext.push(0); // host_name
        put_u16(&mut ext, name.len());
        ext.extend_from_slice(name.as_bytes());
    }
    put_u16(&mut ext, 13); // signature_algorithms
    put_u16(&mut ext, 2 + 2 * SIG_ALGS.len());
    put_u16(&mut ext, 2 * SIG_ALGS.len());
    for s in SIG_ALGS {
        ext.extend_from_slice(&s.to_be_bytes());
    }
    put_u16(&mut ext, 10); // elliptic_curves
    put_u16(&mut ext, 2 + 2 * GROUPS.len());
    put_u16(&mut ext, 2 * GROUPS.len());
    for g in GROUPS {
        ext.extend_from_slice(&g.to_be_bytes());
    }
    ext
}

/// The bytes writecb() writes: the ClientHello in handshake records of at
/// most 0x4000 bytes, each with record version 3.0 whatever the hello asks
/// for (msgbuf_final(), sslgetcert.c:439-470).
pub fn client_hello(sni: Option<&str>, version: (u8, u8), random: &[u8; 32]) -> Vec<u8> {
    let mut body = vec![version.0, version.1];
    body.extend_from_slice(random);
    body.push(0); // add_sessionid(): empty
    put_u16(&mut body, 2 * CIPHERS.len());
    for c in CIPHERS {
        body.extend_from_slice(&c.to_be_bytes());
    }
    body.extend_from_slice(&COMPRESSION);
    let ext = extensions(sni);
    put_u16(&mut body, ext.len());
    body.extend_from_slice(&ext);

    // hsbuf_final()
    let mut hs = vec![HS_CLIENT_HELLO];
    hs.extend_from_slice(&(body.len() as u32).to_be_bytes()[1..]);
    hs.extend_from_slice(&body);

    let mut out = Vec::with_capacity(hs.len() + 5);
    for chunk in hs.chunks(0x4000) {
        out.extend_from_slice(&[MSG_HANDSHAKE, 3, 0]);
        put_u16(&mut out, chunk.len());
        out.extend_from_slice(chunk);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The 154 bytes the reference probe (fw 5130) sent for
    /// `evsslgetcert -6 -h www.ripe.net -A 15001 www.ripe.net` in the twin
    /// run of 2026-10-13 (probe-ref.pcap, port 43042). Its random starts
    /// with 0x6acdc98e, the RESULT's "time":1791871374.
    pub(crate) const REF_HELLO_WWW_RIPE_NET: &str = "16030000950100009103036acdc98e706f367636706729c24ac54be62734996e886bfcf053f5f5a8a9cbe6000024130113031302c02bc02fcca9cca8c02cc030c00ac009c013c014009c009d002f0035000a020100004300000011000f00000c7777772e726970652e6e6574000d0018001604030503060308040805080604010501060102030201000a000e000c001d00170018001901000101";

    fn ref_random() -> [u8; 32] {
        let hello = hex::decode(REF_HELLO_WWW_RIPE_NET).unwrap();
        hello[11..43].try_into().unwrap()
    }

    /// 2026-10-13: Starla's hello for the same line was 1471 bytes of rustls
    /// (record 0301, supported_versions 0304/0303, X25519MLKEM768).
    #[test]
    fn the_hello_is_the_references_byte_for_byte() {
        let hello = client_hello(Some("www.ripe.net"), TLS_1_2, &ref_random());
        assert_eq!(hex::encode(&hello), REF_HELLO_WWW_RIPE_NET);
    }

    /// atlas.ripe.net (15002) was 156 bytes: the SNI is the only part that
    /// depends on the line.
    #[test]
    fn the_sni_is_the_only_variable_part() {
        let hello = client_hello(Some("atlas.ripe.net"), TLS_1_2, &ref_random());
        assert_eq!(hello.len(), 156);
        assert_eq!(&hello[..5], &[0x16, 0x03, 0x00, 0x00, 0x97]);
    }

    /// Without -h sslgetcert sends no server_name extension (state->sni is
    /// only ever h_arg, sslgetcert.c:966 and 695).
    #[test]
    fn without_h_there_is_no_server_name() {
        let hello = client_hello(None, TLS_1_2, &ref_random());
        let with = client_hello(Some("www.ripe.net"), TLS_1_2, &ref_random());
        assert_eq!(hello.len(), with.len() - 21);
        // extensions: signature_algorithms first, then supported_groups
        let ext_off = 5 + 4 + 2 + 32 + 1 + 2 + 36 + 3;
        assert_eq!(&hello[ext_off..ext_off + 4], &[0x00, 46, 0x00, 13]);
    }

    #[test]
    fn the_random_starts_with_the_time() {
        let r = hello_random(1791871374);
        assert_eq!(&r[..4], &[0x6a, 0xcd, 0xc9, 0x8e]);
    }
}
