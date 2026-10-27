//! Outbound URLs a person typed: a webhook, an OpenAI-compatible base URL,
//! the hosted brain's URL. On a farm's own machine the owner already
//! controls the network, so anything goes. In hosted mode (the paid hosted
//! service, `OPENPASTURE_HOSTED=1`), the machine isn't the farmer's: such a
//! URL may only reach the public internet, never this host, its private
//! network or a cloud metadata address. Checked when the URL is saved and
//! again before each request (a name can be pointed elsewhere later).

use std::net::IpAddr;
use std::sync::OnceLock;

/// Set to `1` (or `true`) on the hosted service.
pub const HOSTED_ENV: &str = "OPENPASTURE_HOSTED";

/// This server runs as the hosted service.
pub fn hosted() -> bool {
    static HOSTED: OnceLock<bool> = OnceLock::new();
    *HOSTED.get_or_init(|| std::env::var(HOSTED_ENV).is_ok_and(|v| matches!(v.trim(), "1" | "true" | "yes")))
}

/// A public internet address: not loopback, private, link-local (cloud
/// metadata), carrier-grade NAT, multicast, documentation or unspecified.
pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            !(v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_multicast()
                || v4.is_documentation()
                || a == 0
                || (a == 100 && (64..128).contains(&b))
                || (a == 192 && b == 0)
                || (a == 198 && (18..20).contains(&b))
                || a >= 240)
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_public(IpAddr::V4(v4));
            }
            let first = v6.segments()[0];
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (first & 0xfe00) == 0xfc00
                || (first & 0xffc0) == 0xfe80
                || first == 0x2001 && v6.segments()[1] == 0x0db8)
        }
    }
}

/// `scheme://host:port/…` → (host without brackets, port).
fn host_port(url: &str) -> Option<(String, u16)> {
    let (scheme, rest) = url.split_once("://")?;
    let default = match scheme.to_ascii_lowercase().as_str() {
        "https" => 443,
        "http" => 80,
        _ => return None,
    };
    let authority = rest.split(['/', '?', '#']).next()?;
    let authority = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    if let Some(v6) = authority.strip_prefix('[') {
        let (host, after) = v6.split_once(']')?;
        let port = after.strip_prefix(':').map(|p| p.parse().ok()).unwrap_or(Some(default))?;
        return Some((host.to_owned(), port));
    }
    match authority.rsplit_once(':') {
        Some((h, p)) => Some((h.to_owned(), p.parse().ok()?)),
        None => Some((authority.to_owned(), default)),
    }
}

/// Whether `url` reaches only public addresses (every address its name has).
pub async fn check_public(url: &str) -> Result<(), String> {
    let (host, port) = host_port(url.trim()).ok_or_else(|| format!("{url} isn't an http(s) URL."))?;
    let host = host.to_ascii_lowercase();
    if host.is_empty() || host == "localhost" || host.ends_with(".localhost") || host.ends_with(".internal") {
        return Err(format!("{host} isn't on the public internet."));
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        return if is_public(ip) { Ok(()) } else { Err(format!("{host} isn't on the public internet.")) };
    }
    let addrs: Vec<IpAddr> =
        tokio::net::lookup_host((host.as_str(), port)).await.map_err(|e| format!("{host} doesn't resolve: {e}"))?.map(|a| a.ip()).collect();
    if addrs.is_empty() || addrs.iter().any(|ip| !is_public(*ip)) {
        return Err(format!("{host} isn't on the public internet."));
    }
    Ok(())
}

/// In hosted mode, [`check_public`]; otherwise anything goes.
pub async fn guard_url(url: &str) -> Result<(), String> {
    if !hosted() {
        return Ok(());
    }
    check_public(url).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_and_metadata_addresses_are_not_public() {
        for ip in
            ["127.0.0.1", "10.1.2.3", "172.16.0.1", "192.168.1.5", "169.254.169.254", "100.64.0.1", "0.0.0.0", "::1", "fd00::1", "fe80::1", "::ffff:10.0.0.1"]
        {
            assert!(!is_public(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["1.1.1.1", "8.8.8.8", "2606:4700:4700::1111"] {
            assert!(is_public(ip.parse().unwrap()), "{ip}");
        }
    }

    #[test]
    fn hosts_and_ports_come_out_of_urls() {
        assert_eq!(host_port("https://hooks.example.com/x?y=1"), Some(("hooks.example.com".into(), 443)));
        assert_eq!(host_port("http://10.0.0.5:8080/v1"), Some(("10.0.0.5".into(), 8080)));
        assert_eq!(host_port("https://[::1]:9000/"), Some(("::1".into(), 9000)));
        assert_eq!(host_port("https://user@169.254.169.254/latest"), Some(("169.254.169.254".into(), 443)));
        assert_eq!(host_port("ftp://x"), None);
    }

    #[tokio::test]
    async fn literal_and_named_private_hosts_are_refused() {
        for url in [
            "https://169.254.169.254/latest/meta-data",
            "http://127.0.0.1:7878/api",
            "https://localhost/x",
            "https://[fd00::1]/",
            "https://metadata.google.internal/",
        ] {
            assert!(check_public(url).await.is_err(), "{url}");
        }
        assert!(check_public("https://1.1.1.1/dns-query").await.is_ok());
    }
}
