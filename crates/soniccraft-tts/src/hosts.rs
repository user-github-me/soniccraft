//! Hosts and URLs: which hosts are on this machine or a private network (they are never retried after a
//! refused connection and can be probed cheaply; only loopback hosts may receive the provider's own API key
//! without `SONICCRAFT_ALLOWED_HOSTS`, see `keys.rs`), URLs without their `user:password@` part for messages,
//! and the TCP reachability probe for local servers.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::time::Duration;

/// `host` lower-cased, without IPv6 brackets or a trailing dot.
fn normalize(host: &str) -> String {
    host.trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim_end_matches('.')
        .to_ascii_lowercase()
}

/// The address `host` names, also for IPv4-mapped IPv6 literals (`::ffff:10.0.0.1` → `10.0.0.1`).
fn ip_of(host: &str) -> Option<IpAddr> {
    match host.parse::<IpAddr>().ok()? {
        IpAddr::V6(v6) => Some(v6.to_ipv4_mapped().map_or(IpAddr::V6(v6), IpAddr::V4)),
        ip => Some(ip),
    }
}

fn is_private_v4(ip: Ipv4Addr) -> bool {
    ip.is_private() || ip.is_link_local()
}

fn is_private_v6(ip: Ipv6Addr) -> bool {
    let first = ip.segments()[0];
    // fc00::/7 (unique local, the IPv6 RFC 1918) and fe80::/10 (link-local).
    (first & 0xfe00) == 0xfc00 || (first & 0xffc0) == 0xfe80
}

/// True for this machine: `localhost`, `*.localhost`, `127.0.0.0/8`, `::1`, and the unspecified addresses
/// `0.0.0.0` / `::` (connecting to them reaches this machine).
pub(crate) fn is_loopback_host(host: &str) -> bool {
    let host = normalize(host);
    if host == "localhost" || host.ends_with(".localhost") {
        return true;
    }
    ip_of(&host).is_some_and(|ip| ip.is_loopback() || ip.is_unspecified())
}

/// True for hosts on this machine or a private network: loopback (see above), RFC 1918 (`10/8`,
/// `172.16/12`, `192.168/16`), link-local (`169.254/16`, `fe80::/10`), IPv6 unique-local (`fc00::/7`) and
/// mDNS names (`*.local`).
pub fn is_local_host(host: &str) -> bool {
    if is_loopback_host(host) {
        return true;
    }
    let host = normalize(host);
    if host.ends_with(".local") {
        return true;
    }
    match ip_of(&host) {
        Some(IpAddr::V4(v4)) => is_private_v4(v4),
        Some(IpAddr::V6(v6)) => is_private_v6(v6),
        None => false,
    }
}

/// The host of `url` (normalized), if it parses.
pub(crate) fn url_host(url: &str) -> Option<String> {
    let parsed = reqwest::Url::parse(url.trim()).ok()?;
    parsed.host_str().map(normalize)
}

/// True when `url` points at this machine or a private network (see [`is_local_host`]).
pub fn is_local_url(url: &str) -> bool {
    url_host(url).is_some_and(|h| is_local_host(&h))
}

/// True when `url` points at this machine.
pub(crate) fn is_loopback_url(url: &str) -> bool {
    url_host(url).is_some_and(|h| is_loopback_host(&h))
}

/// Characters that end the authority part of a URL inside free text.
fn ends_authority(c: char) -> bool {
    c.is_whitespace()
        || matches!(
            c,
            '/' | '?' | '#' | '"' | '\'' | '`' | '<' | '>' | ')' | ',' | ';'
        )
}

/// `text` with the `user:password@` part removed from every URL in it (`http://u:p@host/x` →
/// `http://host/x`). Everything else is kept as written.
pub(crate) fn scrub_urls(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(pos) = rest.find("://") {
        let authority_start = pos + 3;
        out.push_str(&rest[..authority_start]);
        let after = &rest[authority_start..];
        let end = after.find(ends_authority).unwrap_or(after.len());
        let authority = &after[..end];
        match authority.rfind('@') {
            Some(at) => out.push_str(&authority[at + 1..]),
            None => out.push_str(authority),
        }
        rest = &after[end..];
    }
    out.push_str(rest);
    out
}

/// `url` without its `user:password@` part, for messages and hints (credentials in a `base_url` or
/// `KOKORO_BASE_URL` never appear in diagnostics).
pub fn redact_url(url: &str) -> String {
    scrub_urls(url)
}

/// Whether something accepts TCP connections at `base_url`'s host and port within `timeout` (name
/// resolution included). Only loopback and private-network hosts ([`is_local_host`]) are probed; any other
/// host returns `true` at once, without a connection: the probe exists to tell "no local server is
/// running" quickly, not to test the internet. No HTTP is spoken and no key is sent. A URL without a host
/// returns `false`.
pub async fn probe_reachable(base_url: &str, timeout: Duration) -> bool {
    let Ok(url) = reqwest::Url::parse(base_url.trim()) else {
        return false;
    };
    let Some(host) = url.host_str().map(normalize) else {
        return false;
    };
    if !is_local_host(&host) {
        return true;
    }
    let Some(port) = url.port_or_known_default() else {
        return false;
    };
    let attempt = async move {
        let Ok(addresses) = tokio::net::lookup_host((host.as_str(), port)).await else {
            return false;
        };
        for address in addresses {
            if tokio::net::TcpStream::connect(address).await.is_ok() {
                return true;
            }
        }
        false
    };
    tokio::time::timeout(timeout, attempt)
        .await
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_local_and_remote_hosts() {
        for local in [
            "localhost",
            "LOCALHOST",
            "kokoro.localhost",
            "127.0.0.1",
            "127.8.9.10",
            "[::1]",
            "::1",
            "0.0.0.0",
            "10.1.2.3",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.50",
            "169.254.10.10",
            "fd12:3456::1",
            "[fe80::1]",
            "::ffff:192.168.0.2",
            "gpu-box.local",
            "gpu-box.local.",
        ] {
            assert!(is_local_host(local), "{local}");
        }
        for remote in [
            "api.openai.com",
            "example.com",
            "8.8.8.8",
            "172.32.0.1",
            "100.64.0.1",
            "2606:4700::1111",
            "localhost.example.com",
            "evil-localhost",
            "gpu-box",
        ] {
            assert!(!is_local_host(remote), "{remote}");
        }
        assert!(is_loopback_host("127.0.0.1") && !is_loopback_host("10.0.0.1"));
        assert!(is_local_url("http://192.168.1.5:8880/v1"));
        assert!(is_local_url("http://user:pw@[::1]:8880"));
        assert!(!is_local_url("https://api.openai.com/v1"));
        assert!(!is_local_url("not a url"));
        assert!(is_loopback_url("http://localhost:8880"));
        assert!(!is_loopback_url("http://10.0.0.1:8880"));
    }

    #[test]
    fn userinfo_is_removed_from_urls_in_text() {
        assert_eq!(
            redact_url("http://soniccraft:LEAKCHECKpassw0rd@127.0.0.1:9"),
            "http://127.0.0.1:9"
        );
        assert_eq!(
            redact_url("http://apikey:sk_3f9a@127.0.0.1:9/v1/audio/speech?x=1"),
            "http://127.0.0.1:9/v1/audio/speech?x=1"
        );
        assert_eq!(
            redact_url("http://u:p@[::1]:8880/v1"),
            "http://[::1]:8880/v1"
        );
        assert_eq!(
            redact_url("https://api.openai.com/v1"),
            "https://api.openai.com/v1"
        );
        assert_eq!(
            scrub_urls(
                "error sending request for url (http://a:b@h:1/x): nothing answered at http://c:d@h:1; see https://docs.example/a@b"
            ),
            "error sending request for url (http://h:1/x): nothing answered at http://h:1; see https://docs.example/a@b"
        );
        assert_eq!(scrub_urls("no urls here @ all"), "no urls here @ all");
        assert_eq!(scrub_urls("trailing http://"), "trailing http://");
    }

    #[tokio::test]
    async fn probe_connects_only_to_local_hosts() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let timeout = Duration::from_millis(300);
        assert!(probe_reachable(&format!("http://127.0.0.1:{port}/v1"), timeout).await);
        assert!(probe_reachable(&format!("http://localhost:{port}"), timeout).await);
        // Refused at once: port 1 is served on no test machine (unlike a freed ephemeral port, which a test
        // running in parallel can be handed at once).
        let started = std::time::Instant::now();
        assert!(!probe_reachable("http://127.0.0.1:1", timeout).await);
        assert!(started.elapsed() < Duration::from_millis(250));
        // Silently dropped (macOS drops connection attempts to a bound socket that does not listen; Linux
        // refuses them): unreachable either way, within the timeout.
        let silent = tokio::net::TcpSocket::new_v4().unwrap();
        silent.bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let silent_port = silent.local_addr().unwrap().port();
        let started = std::time::Instant::now();
        assert!(!probe_reachable(&format!("http://127.0.0.1:{silent_port}"), timeout).await);
        assert!(started.elapsed() < Duration::from_secs(2));
        // Remote hosts are never probed: TEST-NET-3 would black-hole a real connection attempt.
        let started = std::time::Instant::now();
        assert!(probe_reachable("http://203.0.113.7:8880", Duration::from_secs(30)).await);
        assert!(started.elapsed() < Duration::from_millis(100));
        assert!(!probe_reachable("not a url", timeout).await);
    }
}
