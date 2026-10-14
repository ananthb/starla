//! Plain-HTTP GET as the reference probe's evhttpget does it
//! (probe-busybox/eperd/httpget.c), for `http://` URLs.
//!
//! RIPE learns a probe's public addresses from its hourly built-ins to
//! ip-echo.ripe.net (12027 over IPv4, 13027 over IPv6, both with
//! `--store-headers 1024`): the reference reports the response's raw header
//! lines, among them `X-Client-IP: <the address the server saw>`. Through
//! reqwest Starla could not report raw header lines (names lowercased, order
//! not kept), and it ignored `-4`/`-6` when resolving. So this client speaks
//! HTTP/1.x itself and reports exactly what httpget reports.

use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// httpget's default User-Agent.
pub const USER_AGENT: &str = "httpget for atlas.ripe.net";

/// Stop reading a response after this many bytes (built-ins fetch a 14-byte
/// echo and a favicon; this only bounds a misbehaving server).
const MAX_RESPONSE_BYTES: usize = 1 << 20;

/// An `http://` URL split as httpget's parse_url splits it.
#[derive(Debug, Clone, PartialEq)]
pub struct HttpUrl {
    /// The authority exactly as written, used for the Host header
    pub hostport: String,
    /// The host without brackets
    pub host: String,
    pub port: u16,
    /// "/" when the URL has none
    pub path: String,
}

pub fn parse_http_url(url: &str) -> anyhow::Result<HttpUrl> {
    let rest = url
        .get(..7)
        .filter(|p| p.eq_ignore_ascii_case("http://"))
        .map(|_| &url[7..])
        .ok_or_else(|| anyhow::anyhow!("not an http:// URL: {url}"))?;
    let (hostport, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    if hostport.is_empty() {
        anyhow::bail!("missing host part in url '{url}'");
    }
    let (host, port) = if let Some(inner) = hostport.strip_prefix('[') {
        let end = inner
            .find(']')
            .ok_or_else(|| anyhow::anyhow!("unterminated [ in url '{url}'"))?;
        let port = match &inner[end + 1..] {
            "" => 80,
            p => p
                .strip_prefix(':')
                .and_then(|p| p.parse().ok())
                .ok_or_else(|| anyhow::anyhow!("bad port in url '{url}'"))?,
        };
        (inner[..end].to_string(), port)
    } else {
        match hostport.rsplit_once(':') {
            Some((h, p)) => (
                h.to_string(),
                p.parse()
                    .map_err(|_| anyhow::anyhow!("bad port in url '{url}'"))?,
            ),
            None => (hostport.to_string(), 80),
        }
    };
    Ok(HttpUrl {
        hostport: hostport.to_string(),
        host,
        port,
        path: path.to_string(),
    })
}

/// The request httpget writes for a GET.
pub fn request(url: &HttpUrl, http10: bool) -> String {
    format!(
        "GET {} HTTP/1.{}\r\nHost: {}\r\nConnection: close\r\nUser-Agent: {}\r\n\r\n",
        url.path,
        if http10 { '0' } else { '1' },
        url.hostport,
        USER_AGENT
    )
}

/// What httpget reports for one fetch.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RawHttpOutcome {
    /// DNS time in ms, only when the host was a name (httpget's ttr)
    pub ttr: Option<f64>,
    pub af: u8,
    pub dst_addr: Option<IpAddr>,
    pub src_addr: Option<IpAddr>,
    /// Connect start to end of body, ms
    pub rt: f64,
    pub res: u16,
    /// The response's version, e.g. "1.0"
    pub ver: String,
    /// Header-line bytes including their line ends, status line and the
    /// closing blank line not counted
    pub hsize: usize,
    /// Body bytes (dechunked)
    pub bsize: usize,
    /// The stored header entries, `--store-headers` budget applied
    pub header: Option<Vec<String>>,
    /// httpget's "err" text, when the fetch failed after resolution
    pub err: Option<String>,
    /// The response was read to the end (httpget's READ_DONE)
    pub done: bool,
}

/// Fetch `url` as evhttpget does. `af` is 4, 6, or 0 for either; resolution
/// failure is an error (no result), as Starla treated it before.
pub async fn fetch(
    url: &HttpUrl,
    af: u8,
    http10: bool,
    store_headers: usize,
    timeout: Duration,
) -> anyhow::Result<RawHttpOutcome> {
    let (dst, ttr) = match url.host.parse::<IpAddr>() {
        Ok(ip) => {
            // A literal host must still honour -4/-6: evhttpget resolves with
            // the requested family, so an IPv4 literal under -6 is a
            // resolution failure, not an IPv4 connect.
            if (af == 4 && ip.is_ipv6()) || (af == 6 && ip.is_ipv4()) {
                anyhow::bail!("no IPv{af} address for {}", url.host);
            }
            (SocketAddr::new(ip, url.port), None)
        }
        Err(_) => {
            let t0 = Instant::now();
            let addrs = tokio::time::timeout(
                timeout,
                tokio::net::lookup_host((url.host.as_str(), url.port)),
            )
            .await
            .map_err(|_| anyhow::anyhow!("dns timeout for {}", url.host))??;
            let dst = addrs
                .into_iter()
                .find(|a| match af {
                    4 => a.is_ipv4(),
                    6 => a.is_ipv6(),
                    _ => true,
                })
                .ok_or_else(|| anyhow::anyhow!("no IPv{af} address for {}", url.host))?;
            (dst, Some(t0.elapsed().as_secs_f64() * 1000.0))
        }
    };

    let mut out = RawHttpOutcome {
        ttr,
        af: if dst.is_ipv4() { 4 } else { 6 },
        dst_addr: Some(dst.ip()),
        ..Default::default()
    };

    let start = Instant::now();
    let mut stream = match tokio::time::timeout(timeout, TcpStream::connect(dst)).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            out.err = Some(format!("connect: {e}"));
            return Ok(out);
        }
        Err(_) => {
            out.err = Some("connect: timeout".into());
            return Ok(out);
        }
    };
    out.src_addr = stream.local_addr().ok().map(|a| a.ip());

    let read = async {
        stream.write_all(request(url, http10).as_bytes()).await?;
        let mut buf = Vec::with_capacity(4096);
        let mut chunk = [0u8; 8192];
        loop {
            let n = stream.read(&mut chunk).await?;
            if n == 0 || buf.len() >= MAX_RESPONSE_BYTES {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
        }
        Ok::<_, std::io::Error>(buf)
    };
    let raw = match tokio::time::timeout(timeout, read).await {
        Ok(Ok(raw)) => raw,
        Ok(Err(e)) => {
            out.err = Some(format!("error reading response: {e}"));
            return Ok(out);
        }
        Err(_) => {
            out.err = Some("timeout reading status".into());
            return Ok(out);
        }
    };
    out.rt = start.elapsed().as_secs_f64() * 1000.0;

    match parse_response(&raw, store_headers) {
        Ok(p) => {
            out.res = p.res;
            out.ver = p.ver;
            out.hsize = p.hsize;
            out.bsize = p.bsize;
            out.header = p.header;
            out.done = true;
        }
        Err(e) => out.err = Some(e),
    }
    Ok(out)
}

#[derive(Debug, Clone, PartialEq)]
pub struct ParsedResponse {
    pub res: u16,
    pub ver: String,
    pub hsize: usize,
    pub bsize: usize,
    pub header: Option<Vec<String>>,
}

/// Parse a complete response the way httpget's read loop does: the status
/// line, header lines counted into hsize (with their line ends), then the
/// body by Content-Length, chunked encoding, or to the end.
pub fn parse_response(raw: &[u8], store_headers: usize) -> Result<ParsedResponse, String> {
    let mut pos = 0;
    let next_line = |pos: &mut usize| -> Option<(String, usize)> {
        let rest = raw.get(*pos..)?;
        let nl = rest.iter().position(|&b| b == b'\n')?;
        let len = nl + 1;
        let mut line = &rest[..nl];
        if line.last() == Some(&b'\r') {
            line = &line[..line.len() - 1];
        }
        *pos += len;
        Some((String::from_utf8_lossy(line).into_owned(), len))
    };

    let (status, _) = next_line(&mut pos).ok_or("bad status line: no line")?;
    let rest = status
        .get(..5)
        .filter(|p| p.eq_ignore_ascii_case("HTTP/"))
        .map(|_| &status[5..])
        .ok_or_else(|| format!("bad status line: {status}"))?;
    let (ver, code) = rest
        .split_once(' ')
        .ok_or_else(|| format!("bad status line: {status}"))?;
    let (major, minor) = ver
        .split_once('.')
        .and_then(|(a, b)| Some((a.parse::<u8>().ok()?, b.parse::<u8>().ok()?)))
        .ok_or_else(|| format!("bad status line: {status}"))?;
    let res: u16 = code
        .trim_start()
        .get(..3)
        .and_then(|c| c.parse().ok())
        .ok_or_else(|| format!("bad status line: {status}"))?;

    let mut lines = Vec::new();
    let mut hsize = 0;
    let mut content_length: Option<usize> = None;
    let mut chunked = false;
    loop {
        let (line, len) = next_line(&mut pos).ok_or("bad header line: truncated")?;
        if line.is_empty() {
            break;
        }
        hsize += len;
        if let Some((name, value)) = line.split_once(':') {
            let name = name.trim();
            if name.eq_ignore_ascii_case("Content-Length") {
                content_length = value.trim().parse().ok();
            } else if name.eq_ignore_ascii_case("Transfer-Encoding")
                && value.trim().to_ascii_lowercase().starts_with("chunked")
            {
                chunked = true;
            }
        }
        lines.push(line);
    }

    let body = &raw[pos..];
    let bsize = if chunked {
        dechunked_len(body)
    } else {
        content_length.map_or(body.len(), |n| n.min(body.len()))
    };

    Ok(ParsedResponse {
        res,
        ver: format!("{major}.{minor}"),
        hsize,
        bsize,
        header: stored_headers(&lines, store_headers),
    })
}

fn dechunked_len(mut body: &[u8]) -> usize {
    let mut total = 0;
    while let Some(nl) = body.iter().position(|&b| b == b'\n') {
        let size_line = String::from_utf8_lossy(&body[..nl]);
        let size = usize::from_str_radix(size_line.trim().split(';').next().unwrap_or(""), 16);
        let Ok(size) = size else { break };
        if size == 0 {
            break;
        }
        let data = nl + 1;
        let take = size.min(body.len().saturating_sub(data));
        total += take;
        body = body.get(data + take..).unwrap_or(&[]);
        // the chunk's own CRLF
        body = body
            .strip_prefix(b"\r\n")
            .or(body.strip_prefix(b"\n"))
            .unwrap_or(body);
    }
    total
}

/// httpget's `--store-headers` budget: a line goes in whole while the lines so
/// far plus it and one byte fit; the first that does not is cut to the space
/// left and marked "[...]"; after that nothing. A closing "" follows when the
/// budget was not overrun. None when headers are not stored.
pub fn stored_headers(lines: &[String], max: usize) -> Option<Vec<String>> {
    if max == 0 {
        return None;
    }
    let mut out = Vec::new();
    let mut tot = 0usize;
    for line in lines {
        let len = line.len();
        // httpget: tot_headers+len+1 <= max_headers
        if tot + len < max {
            out.push(line.clone());
            tot += len;
        } else if tot <= max {
            let mut keep = max - tot;
            while !line.is_char_boundary(keep) {
                keep -= 1;
            }
            out.push(format!("{}[...]", &line[..keep]));
            tot += len + 1;
        }
    }
    if tot <= max {
        out.push(String::new());
    }
    Some(out)
}

/// httpget's add_str_quoted: `"` and `\` escaped, printable ASCII as is,
/// every other byte as \u00XX.
pub fn quoted(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for &b in s.as_bytes() {
        match b {
            b'"' | b'\\' => {
                out.push('\\');
                out.push(b as char);
            }
            0x20..=0x7e => out.push(b as char),
            _ => out.push_str(&format!("\\u{b:04x}")),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ip-echo.ripe.net's answer to reference probe 1000004 (13027, from
    /// RIPE's archive): the header list and sizes it stored.
    const ECHO: &[u8] = b"HTTP/1.0 200 OK\r\nX-Client-IP: 2001:418:1::33\r\nContent-Type: text/plain\r\nAccess-Control-Allow-Origin: *\r\nServer: BigIP\r\nConnection: close\r\nContent-Length: 14\r\n\r\n2001:418:1::33";

    #[test]
    fn an_echo_answer_is_reported_as_the_reference_stored_it() {
        let p = parse_response(ECHO, 1024).unwrap();
        assert_eq!(
            (p.res, p.ver.as_str(), p.hsize, p.bsize),
            (200, "1.0", 141, 14)
        );
        assert_eq!(
            p.header.unwrap(),
            [
                "X-Client-IP: 2001:418:1::33",
                "Content-Type: text/plain",
                "Access-Control-Allow-Origin: *",
                "Server: BigIP",
                "Connection: close",
                "Content-Length: 14",
                "",
            ]
        );
        assert_eq!(parse_response(ECHO, 0).unwrap().header, None);
    }

    #[test]
    fn the_header_budget_cuts_like_httpget() {
        let lines: Vec<String> = ["Aaaa: 1", "Bbbb: 22", "Cccc: 333"]
            .map(String::from)
            .into();
        // 7+1 fits in 10; 7+8+1 = 16 does not: cut to 3 bytes, then nothing.
        assert_eq!(stored_headers(&lines, 10).unwrap(), ["Aaaa: 1", "Bbb[...]"]);
        // Two lines fit (7+8+1 <= 16), the third is cut to the 1 byte left.
        assert_eq!(
            stored_headers(&lines, 16).unwrap(),
            ["Aaaa: 1", "Bbbb: 22", "C[...]"]
        );
        // Without a third line the budget holds, so the closing "" follows.
        assert_eq!(
            stored_headers(&lines[..2], 16).unwrap(),
            ["Aaaa: 1", "Bbbb: 22", ""]
        );
        assert_eq!(
            stored_headers(&lines, 1024).unwrap(),
            ["Aaaa: 1", "Bbbb: 22", "Cccc: 333", ""]
        );
    }

    #[test]
    fn urls_split_as_parse_url_splits_them() {
        let u = parse_http_url("http://[2001:67c:2e8:16::c100:68d]").unwrap();
        assert_eq!(
            (
                u.hostport.as_str(),
                u.host.as_str(),
                u.port,
                u.path.as_str()
            ),
            (
                "[2001:67c:2e8:16::c100:68d]",
                "2001:67c:2e8:16::c100:68d",
                80,
                "/"
            )
        );
        let u = parse_http_url("http://www.ripe.net/favicon.ico").unwrap();
        assert_eq!(
            (u.host.as_str(), u.port, u.path.as_str()),
            ("www.ripe.net", 80, "/favicon.ico")
        );
        let u = parse_http_url("http://193.0.6.141:8080/x?y=1").unwrap();
        assert_eq!(
            (u.hostport.as_str(), u.port, u.path.as_str()),
            ("193.0.6.141:8080", 8080, "/x?y=1")
        );
        assert!(parse_http_url("https://x/").is_err());
    }

    #[test]
    fn the_request_is_httpgets() {
        let u = parse_http_url("http://[2001:67c:2e8:16::c100:68d]").unwrap();
        assert_eq!(
            request(&u, false),
            "GET / HTTP/1.1\r\nHost: [2001:67c:2e8:16::c100:68d]\r\nConnection: \
             close\r\nUser-Agent: httpget for atlas.ripe.net\r\n\r\n"
        );
        assert!(request(&u, true).starts_with("GET / HTTP/1.0\r\n"));
    }

    #[test]
    fn chunked_and_unframed_bodies_are_sized() {
        let chunked = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\nabcd\r\n3\r\nefg\r\n0\r\n\r\n";
        assert_eq!(parse_response(chunked, 0).unwrap().bsize, 7);
        let unframed = b"HTTP/1.1 404 Not Found\r\nServer: x\r\n\r\nhello";
        let p = parse_response(unframed, 0).unwrap();
        assert_eq!((p.res, p.bsize, p.hsize), (404, 5, 11));
        assert!(parse_response(b"garbage\r\n\r\n", 0).is_err());
    }

    #[test]
    fn quoting_is_add_str_quoteds() {
        assert_eq!(quoted("a\"b\\c"), r#""a\"b\\c""#);
        assert_eq!(quoted("t\tx\u{e9}"), r#""t\u0009x\u00c3\u00a9""#);
    }

    /// Against a local server that answers like ip-echo: the fetch reports
    /// the address the server saw, sizes, version and our real source.
    #[tokio::test]
    async fn a_fetch_reports_what_the_server_said() {
        let server = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = server.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut s, peer) = server.accept().await.unwrap();
            let mut req = vec![0u8; 512];
            let n = s.read(&mut req).await.unwrap();
            let req = String::from_utf8_lossy(&req[..n]).into_owned();
            assert!(req.starts_with("GET /x HTTP/1.1\r\n"), "{req}");
            assert!(
                req.contains("\r\nUser-Agent: httpget for atlas.ripe.net\r\n"),
                "{req}"
            );
            let ip = peer.ip().to_string();
            let reply = format!(
                "HTTP/1.0 200 OK\r\nX-Client-IP: {ip}\r\nContent-Length: {}\r\n\r\n{ip}",
                ip.len()
            );
            s.write_all(reply.as_bytes()).await.unwrap();
        });
        let url = parse_http_url(&format!("http://127.0.0.1:{port}/x")).unwrap();
        let out = fetch(&url, 4, false, 1024, Duration::from_secs(5))
            .await
            .unwrap();
        assert!(out.done && out.err.is_none(), "{out:?}");
        assert_eq!(
            (out.res, out.ver.as_str(), out.bsize, out.af),
            (200, "1.0", 9, 4)
        );
        assert_eq!(out.header.unwrap()[0], "X-Client-IP: 127.0.0.1");
        assert_eq!(out.src_addr, Some("127.0.0.1".parse().unwrap()));
        assert_eq!(out.ttr, None, "a literal host is not resolved");
    }

    /// The live echo server, the URLs of built-ins 12027 and 13027. Network,
    /// so ignored: `cargo test -p starla-measurements -- --ignored
    /// live_ip_echo`.
    #[tokio::test]
    #[ignore]
    async fn live_ip_echo() {
        for (url, af) in [
            ("http://193.0.6.141", 4),
            ("http://[2001:67c:2e8:16::c100:68d]", 6),
        ] {
            let out = fetch(
                &parse_http_url(url).unwrap(),
                af,
                false,
                1024,
                Duration::from_secs(10),
            )
            .await
            .unwrap();
            println!("{url}: {out:?}");
            if out.err.is_some() {
                continue; // no route in that family from here
            }
            let header = out.header.unwrap();
            let echo = header
                .iter()
                .find_map(|h| h.strip_prefix("X-Client-IP: "))
                .expect("X-Client-IP");
            assert_eq!(out.bsize, echo.len(), "the body is the echoed address");
            assert_eq!(header.last().map(String::as_str), Some(""));
        }
    }
}
