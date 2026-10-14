//! evsslgetcert: a TLS server's certificate chain, as the reference probe's
//! sslgetcert gets it (probe-busybox/eperd/sslgetcert.c, firmware 5130).
//!
//! In the twin run of 2026-10-13 the reference recorded the target's self-
//! signed certificate for `evsslgetcert -6 -h www.ripe.net -A 15001
//! www.ripe.net` and Starla uploaded nothing: it went through rustls with
//! certificate validation and failed with "invalid peer certificate:
//! UnknownIssuer". sslgetcert validates nothing — the certificate, whatever
//! it is, is the measurement — and it is not a TLS client: it connects,
//! writes one hand-built ClientHello ([`hello`]), reads the ServerHello and
//! the Certificate message ([`cert`]) and hangs up. This module is that
//! flow, with the reference's timer, error texts and two report formats.

pub mod cert;
pub mod hello;
pub mod resolve;

use crate::traits::Measurement;
use async_trait::async_trait;
use cert::{Event, ReadState, Reader};
use starla_common::{
    MeasurementData, MeasurementId, MeasurementResult, MeasurementType, ProbeId, Timestamp,
};
use std::collections::BTreeMap;
use std::fmt::Write;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Mutex;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::{sleep_until, timeout_at, Instant};

/// CONN_TO (sslgetcert.c:23): tcputil arms one 5 s timer per address before
/// connecting (tcputil.c:260, 93), and it covers the connect, the hello and
/// the certificates.
pub const CONN_TO_MS: u64 = 5000;

/// libevent reads at most EVBUFFER_MAX_READ (4096, buffer.c:2207) bytes per
/// read event.
const READ_CHUNK: usize = 4096;

/// A crontab line, as far as sslgetcert's state goes: its id, target,
/// port, family and -h.
type Line = (u64, String, u16, u8, Option<String>);

/// state->server_cipher outlives a run. The reference allocates one state
/// per crontab line (sslgetcert_init(), sslgetcert.c:954, xzalloc) and only
/// eat_server_hello() writes the cipher (:1452): sslgetcert_start()
/// (:1820-1840), beforeconnect() (:1695-1720) and connected() leave it. So
/// an alert that comes instead of a ServerHello is reported with the
/// cipher of the line's last ServerHello (report_head(), :1259), 0x0000
/// only before the first. Starla builds a Tls each run, so the line's
/// cipher is kept here. Unlike eperd's, the entry of a line that is stopped
/// and scheduled again lives on.
static LAST_CIPHER: Mutex<BTreeMap<Line, u16>> = Mutex::new(BTreeMap::new());

/// Each one-off brings an id of its own: past this many lines the map
/// starts again rather than grow for ever.
const MAX_LINES: usize = 4096;

#[derive(Debug, Clone)]
pub struct TlsConfig {
    /// The positional argument, a name or an address literal: what is
    /// resolved and reported as "dst_name"
    pub target: String,
    /// `-p` (443)
    pub port: u16,
    /// `-4` or `-6`; 0 for neither
    pub af: u8,
    /// `-h`: the server_name to send; without it the hello has none
    pub sni: Option<String>,
    /// atlas_check_addr() refuses private and special addresses, unless
    /// ATLAS_DISABLE_CHECK_ADDR=yes
    pub check_addr: bool,
    /// The per-address timer, CONN_TO
    pub timeout_ms: u64,
    /// Tests: resolve through these nameservers instead of the system's
    pub nameservers: Vec<SocketAddr>,
}

impl TlsConfig {
    pub fn new(target: &str, port: u16, af: u8, sni: Option<String>) -> Self {
        Self {
            target: target.to_string(),
            port,
            af,
            sni,
            check_addr: !resolve::check_addr_disabled(),
            timeout_ms: CONN_TO_MS,
            nameservers: Vec::new(),
        }
    }
}

/// What both report functions print first.
struct Envelope<'a> {
    /// `-A`: without one there is no id, version or lts
    atlas: Option<&'a str>,
    dst_name: &'a str,
    dst_port: u16,
    /// tu_env.ttr in ms; None when the host was an address literal
    ttr: Option<f64>,
}

/// atlas_get_version_json_str() (libbb/atlas_version.c:56-64)
fn version_json() -> String {
    format!(
        "\"fw\":{}, \"mver\": \"2.6.4\"",
        starla_common::FIRMWARE_VERSION
    )
}

fn af_of(ip: IpAddr) -> u8 {
    if ip.is_ipv4() {
        4
    } else {
        6
    }
}

/// report(), sslgetcert.c:1012-1092: the error report. "time" is the
/// measurement's start (state->gstart) and comes only with an id; dst_addr
/// and af come unless resolution failed; `result` is whatever add_str()
/// gathered since the last connect attempt. "lts":0 is the placeholder the
/// results crate fills in.
fn report_body(env: &Envelope, gstart: i64, dst: Option<IpAddr>, result: &str) -> String {
    let mut s = String::new();
    if let Some(id) = env.atlas {
        let _ = write!(
            s,
            "\"id\":\"{id}\", {}, \"lts\":0, \"time\":{gstart}, ",
            version_json()
        );
    }
    let _ = write!(
        s,
        "\"dst_name\":\"{}\", \"dst_port\":\"{}\", ",
        env.dst_name, env.dst_port
    );
    if let Some(ttr) = env.ttr {
        let _ = write!(s, "\"ttr\":{ttr:.6}, ");
    }
    if let Some(dst) = dst {
        let _ = write!(s, "\"dst_addr\":\"{dst}\", \"af\": {}, ", af_of(dst));
    }
    s.push_str(result);
    s
}

/// What report_head() prints after the envelope.
struct Head {
    /// The last record header's version bytes
    version: (u8, u8),
    dst: IpAddr,
    src: String,
    /// Connect start to connected, ms
    ttc: f64,
    /// Connect start to this report, ms
    rt: f64,
    cipher: u16,
}

/// report_head()'s "method" and "ver" (sslgetcert.c:1206-1235). An unknown
/// version prints the bytes as the C char's %d, signed on x86.
fn method_ver(version: (u8, u8)) -> (&'static str, String) {
    match version {
        (3, 3) => ("TLS", "1.2".into()),
        (3, 2) => ("TLS", "1.1".into()),
        (3, 1) => ("TLS", "1.0".into()),
        (3, 0) => ("SSL", "3.0".into()),
        (a, b) => ("(unknown)", format!("{}.{}", a as i8, b as i8)),
    }
}

/// report_head(), sslgetcert.c:1157-1262: the report of an answer, with
/// "time" taken now. The caller appends the alert or the certificates.
fn head_body(env: &Envelope, now: i64, h: &Head) -> String {
    let mut s = String::new();
    if let Some(id) = env.atlas {
        let _ = write!(s, "\"id\":\"{id}\", {}, \"lts\":0, ", version_json());
    }
    let _ = write!(
        s,
        "\"time\":{now}, \"dst_name\":\"{}\", \"dst_port\":\"{}\"",
        env.dst_name, env.dst_port
    );
    if let Some(ttr) = env.ttr {
        let _ = write!(s, ", \"ttr\":{ttr:.6}");
    }
    let (method, ver) = method_ver(h.version);
    let _ = write!(
        s,
        ", \"method\":\"{method}\", \"ver\":\"{ver}\", \"dst_addr\":\"{}\", \"af\": {}, \
         \"src_addr\":\"{}\", \"ttc\": {:.6}, \"rt\": {:.6}, \"server_cipher\": \"0x{:04x}\"",
        h.dst,
        af_of(h.dst),
        h.src,
        h.ttc,
        h.rt,
        h.cipher
    );
    s
}

/// eat_alert()'s addition (sslgetcert.c:1306-1308)
fn alert_suffix(level: u8, description: u8) -> String {
    format!(", \"alert\": {{ \"level\": {level}, \"description\": {description} }}")
}

/// eat_certificate()'s addition (sslgetcert.c:1545-1587)
fn certs_suffix(certs: &[Vec<u8>]) -> String {
    let pems: Vec<String> = certs.iter().map(|c| cert::pem_json(c)).collect();
    format!(", \"cert\":[ {} ]", pems.join(", "))
}

/// strerror()'s text, which tcputil passes to reporterr() (tcputil.c:315,
/// 527): io::Error's Display without its " (os error N)".
fn strerror(e: &std::io::Error) -> String {
    let s = e.to_string();
    match (e.raw_os_error(), s.rfind(" (os error ")) {
        (Some(_), Some(i)) => s[..i].to_string(),
        _ => s,
    }
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// One sslgetcert run.
pub struct Outcome {
    /// The RESULT body (everything inside `RESULT { … }`)
    pub body: String,
    /// The address last tried, if resolution succeeded
    pub dst: Option<IpAddr>,
    pub src: Option<IpAddr>,
    /// state->server_cipher after the run: the last ServerHello's, this
    /// run's or an earlier one's
    pub cipher: u16,
}

/// sslgetcert_start() through report(): resolve, then try the addresses in
/// order until one answers or fails for good. `cipher` is the line's
/// state->server_cipher from its earlier runs.
pub async fn sslgetcert(config: &TlsConfig, atlas: Option<&str>, cipher: u16) -> Outcome {
    let gstart = Timestamp::now().0;
    let literal = config.target.parse::<IpAddr>().is_ok();
    let (resolved, ttr) = resolve::resolve(&config.target, config.af, &config.nameservers).await;
    let env = Envelope {
        atlas,
        dst_name: &config.target,
        dst_port: config.port,
        ttr: (!literal).then_some(ttr),
    };
    let addrs = match resolved {
        Ok(addrs) => addrs,
        // reporterr(TU_DNS_ERR)
        Err(e) => {
            return Outcome {
                body: report_body(&env, gstart, None, &format!("\"dnserr\":\"{e}\"")),
                dst: None,
                src: None,
                cipher,
            }
        }
    };

    let conn_to = Duration::from_millis(config.timeout_ms);
    // state->result: reset by each connect attempt (beforeconnect()), not by
    // an address check
    let mut result = String::new();
    let mut dst = None;
    for ip in addrs {
        let deadline = Instant::now() + conn_to;
        dst = Some(ip);
        if config.check_addr && !resolve::addr_allowed(ip) {
            // reporterr(TU_BAD_ADDR): no other address is tried
            result.push_str("\"error\": \"address not allowed\"");
            break;
        }

        // beforeconnect()
        result.clear();
        let start = Instant::now();
        let mut stream = match timeout_at(deadline, TcpStream::connect((ip, config.port))).await {
            Ok(Ok(s)) => s,
            // reporterr(TU_CONNECT_ERR) / timeout_callback() while
            // connecting: the next address, or report() when none is left
            Ok(Err(e)) => {
                let _ = write!(result, "\"err\":\"connect: {}\"", strerror(&e));
                continue;
            }
            Err(_) => {
                result.push_str("\"err\":\"connect: timeout\"");
                continue;
            }
        };

        // connected(), then writecb()'s WRITE_HELLO
        let src = stream.local_addr().ok().map(|a| a.ip());
        let src_str = src.map(|a| a.to_string()).unwrap_or_default();
        let ttc = ms(start.elapsed());
        let now = Timestamp::now().0 as u32;
        let hello = hello::client_hello(
            config.sni.as_deref(),
            hello::TLS_1_2,
            &hello::hello_random(now),
        );
        // A write error is ignored (sslgetcert.c:1655-1657): the read
        // timeout reports it
        let _ = timeout_at(deadline, stream.write_all(&hello)).await;

        let mut reader = Reader::new();
        reader.server_cipher = cipher;
        let mut buf = vec![0u8; READ_CHUNK];
        let event = loop {
            match timeout_at(deadline, stream.read(&mut buf)).await {
                Ok(Ok(n)) if n > 0 => {
                    reader.feed(&buf[..n]);
                    match reader.readcb() {
                        Event::Wait => continue,
                        ev => break Some(ev),
                    }
                }
                // End of stream or a read error: tcputil's TU_READ_ERR goes to
                // err_reading(), which handles nothing, so the reference
                // reports when its timer fires — with the socket still open.
                Ok(_) => {
                    sleep_until(deadline).await;
                    break None;
                }
                Err(_) => break None,
            }
        };
        let body = match event {
            Some(Event::Err(err)) => {
                report_body(&env, gstart, Some(ip), &format!("\"err\":\"{err}\""))
            }
            Some(ev @ (Event::Alert { .. } | Event::Certs(_))) => {
                let head = Head {
                    version: (reader.recv_major, reader.recv_minor),
                    dst: ip,
                    src: src_str,
                    ttc,
                    rt: ms(start.elapsed()),
                    cipher: reader.server_cipher,
                };
                let mut body = head_body(&env, Timestamp::now().0, &head);
                match ev {
                    Event::Alert { level, description } => {
                        body.push_str(&alert_suffix(level, description))
                    }
                    Event::Certs(certs) => body.push_str(&certs_suffix(&certs)),
                    _ => {}
                }
                body
            }
            // timeout_callback(), sslgetcert.c:752-776
            _ => {
                let _ = write!(result, "\"src_addr\":\"{src_str}\", \"ttc\": {ttc:.6}, ");
                result.push_str(if reader.state == ReadState::Certs {
                    "\"err\":\"timeout reading certificates\""
                } else {
                    "\"err\":\"timeout reading hello\""
                });
                report_body(&env, gstart, Some(ip), &result)
            }
        };
        // tu_cleanup() closes the socket when the report is out
        drop(stream);
        return Outcome {
            body,
            dst: Some(ip),
            src,
            cipher: reader.server_cipher,
        };
    }

    // TU_OUT_OF_ADDRS or TU_BAD_ADDR: report() with the last address tried
    Outcome {
        body: report_body(&env, gstart, dst, &result),
        dst,
        src: None,
        cipher,
    }
}

pub struct Tls {
    pub config: TlsConfig,
    pub probe_id: ProbeId,
    pub msm_id: MeasurementId,
}

#[async_trait]
impl Measurement for Tls {
    fn measurement_type(&self) -> starla_common::MeasurementType {
        starla_common::MeasurementType::Tls
    }

    async fn execute(&self) -> anyhow::Result<MeasurementResult> {
        // -A is the id; a line without one reports no id, version or lts
        let atlas = (self.msm_id.0 != 0).then(|| self.msm_id.0.to_string());
        let c = &self.config;
        let line: Line = (self.msm_id.0, c.target.clone(), c.port, c.af, c.sni.clone());
        let last = LAST_CIPHER
            .lock()
            .ok()
            .and_then(|m| m.get(&line).copied())
            .unwrap_or(0);
        let out = sslgetcert(c, atlas.as_deref(), last).await;
        if let Ok(mut m) = LAST_CIPHER.lock() {
            if m.len() >= MAX_LINES && !m.contains_key(&line) {
                m.clear();
            }
            m.insert(line, out.cipher);
        }
        let dst_addr = out.dst.unwrap_or(if self.config.af == 6 {
            IpAddr::V6(Ipv6Addr::UNSPECIFIED)
        } else {
            IpAddr::V4(Ipv4Addr::UNSPECIFIED)
        });

        Ok(MeasurementResult {
            fw: starla_common::FIRMWARE_VERSION,
            measurement_type: MeasurementType::Tls,
            prb_id: self.probe_id,
            msm_id: self.msm_id,
            timestamp: Timestamp::now(),
            af: af_of(dst_addr),
            dst_addr,
            dst_name: Some(self.config.target.clone()),
            src_addr: out.src,
            proto: Some("TCP".to_string()),
            ttl: None,
            size: None,
            data: MeasurementData::FullLine(out.body),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::cert::tests::{ref_cert_der, ref_flight, REF_CERT_JSON};
    use super::resolve::tests::{fake_nameserver, Zone};
    use super::*;
    use base64::engine::general_purpose::STANDARD as BASE64;
    use base64::Engine;
    use rustls::crypto::aws_lc_rs;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivateSec1KeyDer};
    use rustls::server::{ClientHello, ResolvesServerCert};
    use rustls::sign::CertifiedKey;
    use rustls::SupportedProtocolVersion;
    use sha2::Digest;
    use std::sync::{Arc, Mutex};
    use tokio::net::TcpListener;

    /// The reference's upload for 15001 in the twin run of 2026-10-13.
    const REF_RESULT_15001: &str = r#"RESULT { "id":"15001", "fw":5130, "mver": "2.6.4", "lts":34, "time":1791871374, "dst_name":"www.ripe.net", "dst_port":"443", "ttr":1.522416, "method":"TLS", "ver":"1.2", "dst_addr":"2001:db8:7769:6e00::20", "af": 6, "src_addr":"2001:db8:7769:6e00::31", "ttc": 0.257583, "rt": 1.078542, "server_cipher": "0xc02b", "cert":[ "-----BEGIN CERTIFICATE-----\nMIIBezCCASKgAwIBAgIUJGznK1/aEqg4i8BeK/GsR7P5tnQwCgYIKoZIzj0EAwIw\nITEfMB0GA1UEAwwWcmNnZW4gc2VsZiBzaWduZWQgY2VydDAgFw03NTAxMDEwMDAw\nMDBaGA80MDk2MDEwMTAwMDAwMFowITEfMB0GA1UEAwwWcmNnZW4gc2VsZiBzaWdu\nZWQgY2VydDBZMBMGByqGSM49AgEGCCqGSM49AwEHA0IABPjn9OVNMLso+qIU2rFy\nWux8yR/2kn0T+33MDBzJw6c9USGoKioleSgD8cQIEcHmdWZZ0vrHSlQcazKT+azx\ncd6jNjA0MDIGA1UdEQQrMCmCDHd3dy5yaXBlLm5ldIIIcmlwZS5uZXSCD2F0bGFz\nLXR3aW4udGVzdDAKBggqhkjOPQQDAgNHADBEAiADIO992spQJO7WR0ZWYjCMBx+A\nJSyo65WFSg4laYeFeQIgAnd8j4Ioxqk26tzUYdZQX3MQQryz74gcM+sH9n5EnAQ=\n-----END CERTIFICATE-----" ] }"#;

    fn env_15001(ttr: Option<f64>) -> Envelope<'static> {
        Envelope {
            atlas: Some("15001"),
            dst_name: "www.ripe.net",
            dst_port: 443,
            ttr,
        }
    }

    /// The twin's server flight, read and reported with the reference's
    /// timings, is the reference's upload character for character (once the
    /// results crate has put the lts in).
    #[test]
    fn the_twin_flight_reports_as_the_reference_reported_it() {
        let mut reader = Reader::new();
        reader.feed(&ref_flight());
        let Event::Certs(certs) = reader.readcb() else {
            panic!("no certificates");
        };
        let head = Head {
            version: (reader.recv_major, reader.recv_minor),
            dst: "2001:db8:7769:6e00::20".parse().unwrap(),
            src: "2001:db8:7769:6e00::31".into(),
            ttc: 0.257583,
            rt: 1.078542,
            cipher: reader.server_cipher,
        };
        let body = head_body(&env_15001(Some(1.522416)), 1791871374, &head) + &certs_suffix(&certs);
        let line = format!(
            "RESULT {{ {} }}",
            body.replacen("\"lts\":0,", "\"lts\":34,", 1)
        );
        assert_eq!(line, REF_RESULT_15001);
    }

    #[test]
    fn the_error_report_and_the_alert_report() {
        let env = env_15001(Some(12.5));
        assert_eq!(
            report_body(
                &env,
                1791871374,
                None,
                "\"dnserr\":\"non-recoverable failure in name resolution\""
            ),
            "\"id\":\"15001\", \"fw\":5130, \"mver\": \"2.6.4\", \"lts\":0, \"time\":1791871374, \
             \"dst_name\":\"www.ripe.net\", \"dst_port\":\"443\", \"ttr\":12.500000, \
             \"dnserr\":\"non-recoverable failure in name resolution\""
        );
        let head = Head {
            version: (3, 3),
            dst: "193.0.6.139".parse().unwrap(),
            src: "192.0.2.10".into(),
            ttc: 1.0,
            rt: 2.0,
            cipher: 0,
        };
        let env = Envelope {
            ttr: None,
            ..env_15001(None)
        };
        assert_eq!(
            head_body(&env, 7, &head) + &alert_suffix(2, 70),
            "\"id\":\"15001\", \"fw\":5130, \"mver\": \"2.6.4\", \"lts\":0, \"time\":7, \
             \"dst_name\":\"www.ripe.net\", \"dst_port\":\"443\", \"method\":\"TLS\", \
             \"ver\":\"1.2\", \"dst_addr\":\"193.0.6.139\", \"af\": 4, \
             \"src_addr\":\"192.0.2.10\", \"ttc\": 1.000000, \"rt\": 2.000000, \"server_cipher\": \
             \"0x0000\", \"alert\": { \"level\": 2, \"description\": 70 }"
        );
        // Without -A: no id, version or lts, and report() has no time either
        let env = Envelope { atlas: None, ..env };
        assert_eq!(
            report_body(
                &env,
                7,
                Some("::2".parse().unwrap()),
                "\"err\":\"bad type\""
            ),
            "\"dst_name\":\"www.ripe.net\", \"dst_port\":\"443\", \"dst_addr\":\"::2\", \"af\": \
             6, \"err\":\"bad type\""
        );
        assert!(head_body(&env, 7, &head).starts_with("\"time\":7, \"dst_name\""));
        assert_eq!(method_ver((3, 0)), ("SSL", "3.0".into()));
        assert_eq!(method_ver((0x48, 0x54)), ("(unknown)", "72.84".into()));
    }

    /// Replaces the measured numbers so a live run can be compared whole.
    fn mask(body: &str) -> String {
        let mut out = body.to_string();
        for key in ["\"time\":", "\"ttr\":", "\"ttc\": ", "\"rt\": "] {
            if let Some(i) = out.find(key) {
                let start = i + key.len();
                let len = out[start..]
                    .find(|c: char| !(c.is_ascii_digit() || c == '.'))
                    .unwrap_or(out.len() - start);
                out.replace_range(start..start + len, "N");
            }
        }
        out
    }

    fn config(target: &str, port: u16, af: u8, timeout_ms: u64) -> TlsConfig {
        TlsConfig {
            check_addr: false,
            timeout_ms,
            ..TlsConfig::new(target, port, af, Some("www.ripe.net".into()))
        }
    }

    /// Serves the twin's self-signed certificate (with a key that does not
    /// even match it: nothing on our side checks) and notes the
    /// server_name it was asked for.
    #[derive(Debug)]
    struct TwinCert {
        key: Arc<CertifiedKey>,
        sni: Arc<Mutex<Option<String>>>,
    }

    impl ResolvesServerCert for TwinCert {
        fn resolve(&self, hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
            *self.sni.lock().unwrap() = hello.server_name().map(str::to_string);
            Some(self.key.clone())
        }
    }

    /// A rustls server on 127.0.0.1 for one connection. Its P-256 key is
    /// derived from a fixed string, so no key material sits in the tree.
    async fn rustls_server(
        versions: &[&'static SupportedProtocolVersion],
    ) -> (u16, Arc<Mutex<Option<String>>>) {
        let provider = aws_lc_rs::default_provider();
        let scalar = sha2::Sha256::digest(b"starla evsslgetcert test server");
        let mut sec1 = vec![0x30, 0x31, 0x02, 0x01, 0x01, 0x04, 0x20];
        sec1.extend_from_slice(&scalar);
        sec1.extend_from_slice(&[
            0xa0, 0x0a, 0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07,
        ]);
        let key = provider
            .key_provider
            .load_private_key(PrivateKeyDer::Sec1(PrivateSec1KeyDer::from(sec1)))
            .unwrap();
        let sni = Arc::new(Mutex::new(None));
        let resolver = TwinCert {
            key: Arc::new(CertifiedKey::new(
                vec![CertificateDer::from(ref_cert_der())],
                key,
            )),
            sni: sni.clone(),
        };
        let server = rustls::ServerConfig::builder_with_provider(Arc::new(provider))
            .with_protocol_versions(versions)
            .unwrap()
            .with_no_client_auth()
            .with_cert_resolver(Arc::new(resolver));
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            if let Ok((tcp, _)) = listener.accept().await {
                let _ = acceptor.accept(tcp).await;
            }
        });
        (port, sni)
    }

    /// 2026-10-13: Starla's rustls client refused the twin's self-signed
    /// certificate ("invalid peer certificate: UnknownIssuer") and uploaded
    /// nothing. A TLS 1.2-capable rustls server takes sslgetcert's hello,
    /// and its self-signed certificate is the result.
    #[tokio::test]
    async fn a_self_signed_certificate_is_recorded_not_refused() {
        let (port, sni) = rustls_server(&[&rustls::version::TLS12, &rustls::version::TLS13]).await;
        let (ns, _) = fake_nameserver(Zone::Loopback).await;
        let mut cfg = config("www.ripe.net", port, 4, 5000);
        cfg.nameservers = vec![ns];
        let out = sslgetcert(&cfg, Some("15001"), 0).await;
        assert_eq!(
            mask(&out.body),
            format!(
                "\"id\":\"15001\", \"fw\":5130, \"mver\": \"2.6.4\", \"lts\":0, \"time\":N, \
                 \"dst_name\":\"www.ripe.net\", \"dst_port\":\"{port}\", \"ttr\":N, \
                 \"method\":\"TLS\", \"ver\":\"1.2\", \"dst_addr\":\"127.0.0.1\", \"af\": 4, \
                 \"src_addr\":\"127.0.0.1\", \"ttc\": N, \"rt\": N, \"server_cipher\": \
                 \"0xc02b\", \"cert\":[ {REF_CERT_JSON} ]"
            )
        );
        assert_eq!(sni.lock().unwrap().as_deref(), Some("www.ripe.net"));
        assert_eq!(out.dst, Some(IpAddr::V4(Ipv4Addr::LOCALHOST)));
    }

    /// A server that speaks only TLS 1.3 cannot answer a hello without
    /// supported_versions: its protocol_version alert is the result.
    #[tokio::test]
    async fn a_tls13_only_server_answers_with_an_alert() {
        let (port, _) = rustls_server(&[&rustls::version::TLS13]).await;
        let out = sslgetcert(&config("127.0.0.1", port, 4, 5000), Some("15001"), 0).await;
        assert_eq!(
            mask(&out.body),
            format!(
                "\"id\":\"15001\", \"fw\":5130, \"mver\": \"2.6.4\", \"lts\":0, \"time\":N, \
                 \"dst_name\":\"127.0.0.1\", \"dst_port\":\"{port}\", \"method\":\"TLS\", \
                 \"ver\":\"1.2\", \"dst_addr\":\"127.0.0.1\", \"af\": 4, \
                 \"src_addr\":\"127.0.0.1\", \"ttc\": N, \"rt\": N, \"server_cipher\": \
                 \"0x0000\", \"alert\": {{ \"level\": 2, \"description\": 70 }}"
            )
        );
    }

    /// The twin's flight on the first connection, a handshake_failure alert
    /// on the second, on one port, so both runs are the same line.
    async fn flight_then_alert() -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let replies = [ref_flight(), vec![0x15, 0x03, 0x03, 0x00, 0x02, 0x02, 0x28]];
            for reply in replies {
                let Ok((mut tcp, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = [0u8; 512];
                let _ = tcp.read(&mut buf).await;
                let _ = tcp.write_all(&reply).await;
                tokio::spawn(async move { while tcp.read(&mut buf).await.is_ok_and(|n| n > 0) {} });
            }
        });
        port
    }

    fn body_of(result: &MeasurementResult) -> &str {
        match &result.data {
            MeasurementData::FullLine(body) => body,
            // Not `{other:?}`: the result line can hold a certificate, which
            // CodeQL flags as cleartext logging.
            _ => panic!("measurement result is not a full line"),
        }
    }

    /// state->server_cipher is the line's, not the run's: an alert after an
    /// earlier run's ServerHello is reported with that run's cipher, as
    /// eperd reports it; another line starts at 0x0000.
    #[tokio::test]
    async fn the_cipher_of_the_lines_last_server_hello_stays() {
        let port = flight_then_alert().await;
        let tls = |msm_id| Tls {
            config: config("127.0.0.1", port, 4, 2000),
            probe_id: ProbeId(1),
            msm_id: MeasurementId(msm_id),
        };
        let first = tls(990_001).execute().await.unwrap();
        assert!(body_of(&first).contains("\"server_cipher\": \"0xc02b\", \"cert\":["));
        let second = tls(990_001).execute().await.unwrap();
        assert!(
            body_of(&second).ends_with(
                "\"server_cipher\": \"0xc02b\", \"alert\": { \"level\": 2, \"description\": 40 }"
            ),
            "{}",
            body_of(&second)
        );

        let port = flight_then_alert().await;
        let other = |msm_id| Tls {
            config: config("127.0.0.1", port, 4, 2000),
            probe_id: ProbeId(1),
            msm_id: MeasurementId(msm_id),
        };
        // the first run of 990002 on this port: a ServerHello
        let _ = other(990_002).execute().await.unwrap();
        // a line never run before, answered with the alert
        let fresh = other(990_003).execute().await.unwrap();
        assert!(
            body_of(&fresh).contains("\"server_cipher\": \"0x0000\", \"alert\""),
            "{}",
            body_of(&fresh)
        );
    }

    /// A listener that runs `serve` on its one connection.
    async fn tcp_server<F, Fut>(serve: F) -> u16
    where
        F: FnOnce(TcpStream) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send,
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            if let Ok((tcp, _)) = listener.accept().await {
                serve(tcp).await;
            }
        });
        port
    }

    fn timeout_report(port: u16, err: &str) -> String {
        format!(
            "\"id\":\"15001\", \"fw\":5130, \"mver\": \"2.6.4\", \"lts\":0, \"time\":N, \
             \"dst_name\":\"127.0.0.1\", \"dst_port\":\"{port}\", \"dst_addr\":\"127.0.0.1\", \
             \"af\": 4, \"src_addr\":\"127.0.0.1\", \"ttc\": N, \"err\":\"{err}\""
        )
    }

    /// timeout_callback() after the connect: the hello never came.
    #[tokio::test]
    async fn a_silent_server_is_a_timeout_reading_hello() {
        let port = tcp_server(|mut tcp| async move {
            let mut buf = [0u8; 512];
            while tcp.read(&mut buf).await.is_ok_and(|n| n > 0) {}
        })
        .await;
        let t0 = Instant::now();
        let out = sslgetcert(&config("127.0.0.1", port, 4, 300), Some("15001"), 0).await;
        assert!(t0.elapsed() >= Duration::from_millis(300));
        assert_eq!(
            mask(&out.body),
            timeout_report(port, "timeout reading hello")
        );
    }

    /// A ServerHello and then nothing: the reader waits in READ_CERTS.
    #[tokio::test]
    async fn a_server_hello_alone_is_a_timeout_reading_certificates() {
        let port = tcp_server(|mut tcp| async move {
            let mut buf = [0u8; 512];
            let _ = tcp.read(&mut buf).await;
            let flight = ref_flight();
            let mut rec = vec![0x16, 0x03, 0x03, 0x00, 80];
            rec.extend_from_slice(&flight[5..85]);
            let _ = tcp.write_all(&rec).await;
            while tcp.read(&mut buf).await.is_ok_and(|n| n > 0) {}
        })
        .await;
        let out = sslgetcert(&config("127.0.0.1", port, 4, 300), Some("15001"), 0).await;
        assert_eq!(
            mask(&out.body),
            timeout_report(port, "timeout reading certificates")
        );
    }

    /// A server that hangs up is not reported until the timer fires
    /// (TU_READ_ERR goes to err_reading(), which handles nothing), and the
    /// reference keeps its end open until then.
    #[tokio::test]
    async fn a_hang_up_is_reported_when_the_timer_fires() {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let port = tcp_server(|mut tcp| async move {
            let mut buf = [0u8; 512];
            let got = tcp.read(&mut buf).await.unwrap_or(0);
            let t0 = Instant::now();
            let _ = tcp.shutdown().await;
            while tcp.read(&mut buf).await.is_ok_and(|n| n > 0) {}
            let _ = tx.send((got, t0.elapsed()));
        })
        .await;
        let out = sslgetcert(&config("127.0.0.1", port, 4, 400), Some("15001"), 0).await;
        assert_eq!(
            mask(&out.body),
            timeout_report(port, "timeout reading hello")
        );
        let (got, open_for) = rx.await.unwrap();
        // the whole hello: 154 bytes with -h www.ripe.net, as in the twin
        assert_eq!(got, 154);
        assert!(open_for >= Duration::from_millis(300), "{open_for:?}");
    }

    /// reporterr(TU_CONNECT_ERR), then TU_OUT_OF_ADDRS: report() with the
    /// last error and no src_addr.
    // The refused text is Linux's "Connection refused"; Windows differs.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_refused_connect_is_reported_with_strerror() {
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let out = sslgetcert(&config("127.0.0.1", port, 4, 1000), Some("15001"), 0).await;
        assert_eq!(
            mask(&out.body),
            format!(
                "\"id\":\"15001\", \"fw\":5130, \"mver\": \"2.6.4\", \"lts\":0, \"time\":N, \
                 \"dst_name\":\"127.0.0.1\", \"dst_port\":\"{port}\", \"dst_addr\":\"127.0.0.1\", \
                 \"af\": 4, \"err\":\"connect: Connection refused\""
            )
        );
    }

    /// atlas_check_addr() stops a loopback target before any connect.
    #[tokio::test]
    async fn a_loopback_target_is_not_allowed() {
        let cfg = TlsConfig {
            check_addr: true,
            ..config("127.0.0.1", 443, 4, 1000)
        };
        let out = sslgetcert(&cfg, Some("15001"), 0).await;
        assert_eq!(
            mask(&out.body),
            "\"id\":\"15001\", \"fw\":5130, \"mver\": \"2.6.4\", \"lts\":0, \"time\":N, \
             \"dst_name\":\"127.0.0.1\", \"dst_port\":\"443\", \"dst_addr\":\"127.0.0.1\", \
             \"af\": 4, \"error\": \"address not allowed\""
        );
    }

    /// RIPE's stored results of the reference probe (prb_id 1000004, fw
    /// 5130) for 15001 and 15002, as its API served them on 2026-10-13: the
    /// probe's RESULT line with "id" as msm_id and "time" as timestamp, and
    /// prb_id, msm_name, from, type and stored_timestamp added.
    const RIPE_15001: &str = include_str!("testdata/ripe-1000004-15001.json");
    const RIPE_15002: &str = include_str!("testdata/ripe-1000004-15002.json");

    type Object = serde_json::Map<String, serde_json::Value>;

    fn stored(archive: &str) -> Object {
        let v: serde_json::Value = serde_json::from_str(archive).unwrap();
        v[0].as_object().unwrap().clone()
    }

    /// The DER in a stored "cert" string
    fn der_of(pem: &serde_json::Value) -> Vec<u8> {
        let b64: String = pem
            .as_str()
            .unwrap()
            .strip_prefix("-----BEGIN CERTIFICATE-----\n")
            .unwrap()
            .strip_suffix("-----END CERTIFICATE-----")
            .unwrap()
            .split('\n')
            .collect();
        BASE64.decode(b64).unwrap()
    }

    fn stored_certs(ripe: &Object) -> Vec<Vec<u8>> {
        ripe["cert"]
            .as_array()
            .unwrap()
            .iter()
            .map(der_of)
            .collect()
    }

    /// A TLS 1.2 server's first flight to sslgetcert's hello: ServerHello
    /// (a 32-byte session id, `cipher`, null compression, three
    /// extensions), Certificate, ServerKeyExchange and ServerHelloDone, the
    /// handshake cut into records of at most `max_record` bytes.
    fn server_flight(cipher: u16, certs: &[Vec<u8>], max_record: usize) -> Vec<u8> {
        fn u24(n: usize) -> [u8; 3] {
            let b = (n as u32).to_be_bytes();
            [b[1], b[2], b[3]]
        }
        fn message(typ: u8, body: &[u8]) -> Vec<u8> {
            let mut m = vec![typ];
            m.extend_from_slice(&u24(body.len()));
            m.extend_from_slice(body);
            m
        }
        let mut hello = vec![3, 3];
        hello.extend_from_slice(&[0x5a; 32]);
        hello.push(32);
        hello.extend_from_slice(&[0xa5; 32]);
        hello.extend_from_slice(&cipher.to_be_bytes());
        hello.push(0);
        // renegotiation_info, ec_point_formats, extended_master_secret
        let ext = [0xff, 0x01, 0, 1, 0, 0, 0x0b, 0, 2, 1, 0, 0, 0x17, 0, 0];
        hello.extend_from_slice(&(ext.len() as u16).to_be_bytes());
        hello.extend_from_slice(&ext);
        let mut list = Vec::new();
        for c in certs {
            list.extend_from_slice(&u24(c.len()));
            list.extend_from_slice(c);
        }
        let mut certificate = u24(list.len()).to_vec();
        certificate.extend_from_slice(&list);
        let mut key_exchange = vec![3, 0, 0x1d, 32];
        key_exchange.extend_from_slice(&[0x42; 32]);
        key_exchange.extend_from_slice(&[4, 1, 1, 0]);
        key_exchange.extend_from_slice(&[0x99; 256]);
        let mut handshake = message(2, &hello);
        handshake.extend(message(11, &certificate));
        handshake.extend(message(12, &key_exchange));
        handshake.extend(message(14, &[]));
        let mut flight = Vec::new();
        for record in handshake.chunks(max_record) {
            flight.extend_from_slice(&[0x16, 3, 3]);
            flight.extend_from_slice(&(record.len() as u16).to_be_bytes());
            flight.extend_from_slice(record);
        }
        flight
    }

    /// Answers the hello `delay` later with `flight`, in 1400-byte
    /// segments, and holds the connection until sslgetcert hangs up.
    async fn serve_flight(flight: Vec<u8>, delay: Duration) -> u16 {
        tcp_server(move |mut tcp| async move {
            let mut buf = [0u8; 512];
            let _ = tcp.read(&mut buf).await;
            tokio::time::sleep(delay).await;
            for segment in flight.chunks(1400) {
                if tcp.write_all(segment).await.is_err() {
                    return;
                }
                tokio::task::yield_now().await;
            }
            while tcp.read(&mut buf).await.is_ok_and(|n| n > 0) {}
        })
        .await
    }

    /// What RIPE's backend stores for a RESULT body: "id" becomes msm_id
    /// and "time" timestamp, after the probe's other fields, and prb_id,
    /// msm_name, from, type and stored_timestamp come with them.
    fn as_ripe_stores_it(body: &str, ripe: &Object) -> Object {
        let line: Object = serde_json::from_str(&format!("{{ {body} }}")).unwrap();
        let mut out = Object::new();
        let (mut id, mut time) = (None, None);
        for (k, v) in line {
            match k.as_str() {
                "id" => id = Some(v),
                "time" => time = Some(v),
                _ => {
                    out.insert(k, v);
                }
            }
        }
        let msm_id: u64 = id.unwrap().as_str().unwrap().parse().unwrap();
        let from = out["src_addr"].clone();
        out.insert("msm_id".into(), msm_id.into());
        out.insert("prb_id".into(), ripe["prb_id"].clone());
        out.insert("timestamp".into(), time.unwrap());
        out.insert("msm_name".into(), "SSLCert".into());
        out.insert("from".into(), from);
        out.insert("type".into(), "sslcert".into());
        out.insert("stored_timestamp".into(), ripe["stored_timestamp"].clone());
        out
    }

    fn kind(v: &serde_json::Value) -> &'static str {
        match v {
            serde_json::Value::Number(n) if n.is_f64() => "float",
            serde_json::Value::Number(_) => "integer",
            serde_json::Value::String(_) => "string",
            serde_json::Value::Array(_) => "array",
            serde_json::Value::Object(_) => "object",
            serde_json::Value::Bool(_) => "bool",
            serde_json::Value::Null => "null",
        }
    }

    /// Field for field, in RIPE's order, with RIPE's JSON types. `_body` is
    /// kept in the signature for call sites but not printed: the result line
    /// holds a certificate, which CodeQL flags as cleartext logging.
    fn assert_shape(ours: &Object, ripe: &Object, _body: &str) {
        let keys = |o: &Object| o.keys().cloned().collect::<Vec<_>>();
        assert_eq!(keys(ours), keys(ripe), "key order/name mismatch");
        for (k, v) in ripe {
            assert_eq!(kind(&ours[k]), kind(v), "{k}: type mismatch");
        }
    }

    /// RIPE stored the reference's 15001 (www.ripe.net) and 15002
    /// (atlas.ripe.net) with these fields, in this order and of these
    /// types. The same answer — the stored certificates behind a
    /// ServerHello with the stored cipher, in records and segments smaller
    /// than the chain, as a CDN sends them — run through sslgetcert stores
    /// the same object, but for what is measured (times, addresses, lts)
    /// and the test server's port and address family. The certificates are
    /// RIPE's strings character for character.
    #[tokio::test]
    async fn the_line_stores_as_ripe_stored_the_references() {
        for archive in [RIPE_15001, RIPE_15002] {
            let ripe = stored(archive);
            let name = ripe["dst_name"].as_str().unwrap();
            let cipher = ripe["server_cipher"].as_str().unwrap();
            let cipher = u16::from_str_radix(cipher.trim_start_matches("0x"), 16).unwrap();
            let flight = server_flight(cipher, &stored_certs(&ripe), 1300);
            let port = serve_flight(flight, Duration::ZERO).await;
            let (ns, _) = fake_nameserver(Zone::Loopback).await;
            let mut cfg = config(name, port, 4, 5000);
            cfg.sni = Some(name.to_string());
            cfg.nameservers = vec![ns];
            let id = ripe["msm_id"].to_string();
            let out = sslgetcert(&cfg, Some(&id), 0).await;
            let ours = as_ripe_stores_it(&out.body, &ripe);
            assert_shape(&ours, &ripe, &out.body);
            for k in [
                "fw",
                "mver",
                "dst_name",
                "method",
                "ver",
                "server_cipher",
                "cert",
                "msm_id",
                "msm_name",
                "type",
            ] {
                assert_eq!(ours[k], ripe[k], "{k}");
            }
            assert_eq!(ours["dst_port"], port.to_string());
            assert_eq!(ours["af"], 4);
        }
    }

    /// The doc's alert: an object of two integers, and then no "cert".
    /// The reference prints no ttr for an address literal (report_head(),
    /// sslgetcert.c:1200-1204), which the doc does not mark optional.
    #[tokio::test]
    async fn an_alert_for_a_literal_stores_with_alert_and_without_ttr() {
        let ripe = stored(RIPE_15001);
        let port = serve_flight(
            vec![0x15, 0x03, 0x03, 0x00, 0x02, 0x02, 0x28],
            Duration::ZERO,
        )
        .await;
        let out = sslgetcert(&config("127.0.0.1", port, 4, 5000), Some("15001"), 0).await;
        let ours = as_ripe_stores_it(&out.body, &ripe);
        let alert = serde_json::json!({ "level": 2, "description": 40 });
        let mut expected = Object::new();
        for (k, v) in &ripe {
            match k.as_str() {
                "ttr" | "cert" => {}
                "server_cipher" => {
                    expected.insert(k.clone(), v.clone());
                    expected.insert("alert".into(), alert.clone());
                }
                _ => {
                    expected.insert(k.clone(), v.clone());
                }
            }
        }
        assert_shape(&ours, &expected, &out.body);
        assert_eq!(ours["alert"], alert);
        assert_eq!(ours["method"], "TLS");
        assert_eq!(ours["ver"], "1.2");
        assert_eq!(ours["server_cipher"], "0x0000");
    }

    /// Starla's stored 15001 of 2026-10-12 (fw 5120, starla-15001.json) had
    /// "ttc": 0.004238, "rt": 0.01372, "ttr": 0.01372 — seconds, and ttr
    /// the handshake's time. RIPE documents ttr, ttc and rt in ms, and the
    /// reference prints them so with %f (tcputil.c:239, sslgetcert.c:1251-
    /// 1257): ttr is the lookup alone (RIPE stored 5174.370657 against an
    /// rt of 29.03337), ttc the connect, rt the connect's start to the
    /// certificates.
    #[tokio::test]
    async fn times_are_milliseconds_and_ttr_is_the_lookup_alone() {
        let ripe = stored(RIPE_15001);
        // the server answers 150 ms after the hello, the nameserver 600 ms
        // after the query
        let flight = server_flight(0xc030, &stored_certs(&ripe), 16384);
        let port = serve_flight(flight, Duration::from_millis(150)).await;
        let (ns, _) = fake_nameserver(Zone::Slow(Duration::from_millis(600))).await;
        let mut cfg = config("www.ripe.net", port, 4, 5000);
        cfg.nameservers = vec![ns];
        let out = sslgetcert(&cfg, Some("15001"), 0).await;
        let line: Object = serde_json::from_str(&format!("{{ {} }}", out.body)).unwrap();
        let ms = |k: &str| line[k].as_f64().unwrap();
        let (ttr, ttc, rt) = (ms("ttr"), ms("ttc"), ms("rt"));
        assert!((600.0..1500.0).contains(&ttr), "ttr {ttr}");
        assert!((150.0..600.0).contains(&rt), "rt {rt}");
        assert!(ttc < 150.0 && ttc <= rt, "ttc {ttc}");
        for key in ["\"ttr\":", "\"ttc\": ", "\"rt\": "] {
            let at = out.body.find(key).unwrap() + key.len();
            let number: String = out.body[at..]
                .chars()
                .take_while(|c| c.is_ascii_digit() || *c == '.')
                .collect();
            assert_eq!(
                number.split('.').nth(1).map(str::len),
                Some(6),
                "{key}{number}"
            );
        }
    }

    /// A name that does not resolve is reported (it used to be no result):
    /// ttr, then dnserr, and no address.
    #[tokio::test]
    async fn a_name_that_does_not_resolve_is_dnserr() {
        let (ns, _) = fake_nameserver(Zone::Missing).await;
        let mut cfg = config("www.ripe.net", 443, 6, 1000);
        cfg.nameservers = vec![ns];
        let out = sslgetcert(&cfg, Some("15001"), 0).await;
        assert_eq!(
            mask(&out.body),
            "\"id\":\"15001\", \"fw\":5130, \"mver\": \"2.6.4\", \"lts\":0, \"time\":N, \
             \"dst_name\":\"www.ripe.net\", \"dst_port\":\"443\", \"ttr\":N, \
             \"dnserr\":\"nodename nor servname provided, or not known\""
        );
        assert_eq!(out.dst, None);
    }
}
