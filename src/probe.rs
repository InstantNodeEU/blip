use std::error::Error as _;
use std::io::{self, Read};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs, UdpSocket};
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::{Duration, Instant};

use socket2::{Domain, Protocol, Socket, Type};

use crate::config::Target;

pub struct Outcome {
    pub ok: bool,
    pub ms: u32,
    pub detail: String,
}

pub fn run(agent: &ureq::Agent, target: &Target, timeout: Duration) -> Outcome {
    let start = Instant::now();
    let result = match target {
        Target::Http {
            url,
            expect,
            keyword,
        } => http(agent, url, *expect, keyword.as_deref(), timeout),
        Target::Tcp { addr } => tcp(addr, timeout),
        Target::Ping { host } => ping(host, timeout),
        Target::Dns { host } => resolve(host, 0).map(|_| start.elapsed()),
    };
    match result {
        Ok(took) => Outcome {
            ok: true,
            ms: took.as_millis().min(u32::MAX as u128) as u32,
            detail: String::new(),
        },
        Err(mut detail) => {
            if detail.len() > 200 {
                let mut end = 200;
                while !detail.is_char_boundary(end) {
                    end -= 1;
                }
                detail.truncate(end);
            }
            Outcome {
                ok: false,
                ms: 0,
                detail,
            }
        }
    }
}

fn http(
    agent: &ureq::Agent,
    url: &str,
    expect: Option<u16>,
    keyword: Option<&str>,
    timeout: Duration,
) -> Result<Duration, String> {
    let start = Instant::now();
    let resp = match agent.get(url).timeout(timeout).call() {
        Ok(r) => r,
        Err(ureq::Error::Status(_, r)) => r,
        Err(ureq::Error::Transport(t)) => return Err(transport_error(&t)),
    };
    let took = start.elapsed();

    let code = resp.status();
    let good = match expect {
        Some(want) => code == want,
        None => code < 400,
    };
    if !good {
        return Err(format!("HTTP {code}"));
    }

    if let Some(word) = keyword {
        let mut body = Vec::new();
        resp.into_reader()
            .take(1 << 20)
            .read_to_end(&mut body)
            .map_err(|e| format!("reading body: {e}"))?;
        if !contains(&body, word.as_bytes()) {
            return Err(format!("\"{word}\" not found in response"));
        }
    }
    Ok(took)
}

fn transport_error(t: &ureq::Transport) -> String {
    match (t.message(), t.source()) {
        (Some(msg), _) => msg.to_string(),
        (None, Some(src)) => src.to_string(),
        (None, None) => t.kind().to_string(),
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    needle.is_empty() || haystack.windows(needle.len()).any(|w| w == needle)
}

fn resolve(host: &str, port: u16) -> Result<Vec<SocketAddr>, String> {
    let addrs: Vec<_> = (host.trim_matches(['[', ']']), port)
        .to_socket_addrs()
        .map_err(|e| format!("resolving {host}: {e}"))?
        .collect();
    if addrs.is_empty() {
        return Err(format!("{host} has no addresses"));
    }
    Ok(addrs)
}

fn tcp(addr: &str, timeout: Duration) -> Result<Duration, String> {
    let addrs: Vec<_> = addr
        .to_socket_addrs()
        .map_err(|e| format!("resolving {addr}: {e}"))?
        .collect();
    let mut last = format!("{addr} has no addresses");
    for a in addrs {
        let start = Instant::now();
        match TcpStream::connect_timeout(&a, timeout) {
            Ok(_) => return Ok(start.elapsed()),
            Err(e) => last = e.to_string(),
        }
    }
    Err(last)
}

static SEQ: AtomicU16 = AtomicU16::new(0);

/// ICMP echo over an unprivileged datagram socket. Needs no root, but the
/// user's group has to be inside net.ipv4.ping_group_range.
fn ping(host: &str, timeout: Duration) -> Result<Duration, String> {
    let addr = resolve(host, 0)?[0];
    let (domain, proto, request, reply) = match addr {
        SocketAddr::V4(_) => (Domain::IPV4, Protocol::ICMPV4, 8, 0),
        SocketAddr::V6(_) => (Domain::IPV6, Protocol::ICMPV6, 128, 129),
    };
    let sock = Socket::new(domain, Type::DGRAM, Some(proto)).map_err(|e| {
        if e.kind() == io::ErrorKind::PermissionDenied {
            "ping not allowed for this user, see net.ipv4.ping_group_range".to_string()
        } else {
            format!("ping socket: {e}")
        }
    })?;
    let sock: UdpSocket = sock.into();
    sock.connect(addr).map_err(|e| e.to_string())?;

    // The kernel fills in the identifier and checksum for ping sockets.
    let seq = SEQ.fetch_add(1, Ordering::Relaxed).to_be_bytes();
    let packet = [
        request, 0, 0, 0, 0, 0, seq[0], seq[1], b'b', b'l', b'i', b'p',
    ];

    let start = Instant::now();
    sock.send(&packet).map_err(|e| e.to_string())?;
    let mut buf = [0u8; 128];
    loop {
        let left = timeout
            .checked_sub(start.elapsed())
            .filter(|d| !d.is_zero())
            .ok_or("ping timed out")?;
        sock.set_read_timeout(Some(left))
            .map_err(|e| e.to_string())?;
        match sock.recv(&mut buf) {
            Ok(n) if n >= 8 && buf[0] == reply && buf[6..8] == seq => return Ok(start.elapsed()),
            Ok(_) => continue,
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                return Err("ping timed out".into())
            }
            Err(e) => return Err(e.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn finds_keyword() {
        assert!(contains(b"hello world", b"o w"));
        assert!(!contains(b"hello", b"hello!"));
        assert!(contains(b"", b""));
    }

    #[test]
    fn tcp_open_and_closed() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        assert!(tcp(&format!("127.0.0.1:{port}"), Duration::from_secs(1)).is_ok());
        drop(l);
        assert!(tcp(&format!("127.0.0.1:{port}"), Duration::from_secs(1)).is_err());
    }
}
