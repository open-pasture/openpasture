//! Outbound URLs a person typed: a webhook, an OpenAI-compatible base URL,
//! the hosted brain's URL. On a farm's own machine the owner already
//! controls the network, so anything goes. In hosted mode (the paid hosted
//! service, `OPENPASTURE_HOSTED=1`), the machine isn't the farmer's: such a
//! URL may only reach the public internet, never this host, its private
//! network or a cloud metadata address. Checked when the URL is saved and
//! again before each request (a name can be pointed elsewhere later), on the
//! URL as the HTTP client parses it ([`parse`]: one parser, no user info),
//! and at connect time: [`client_builder`]'s clients resolve names to public
//! addresses only and follow no redirects in hosted mode.

use std::net::{IpAddr, Ipv4Addr};
use std::sync::OnceLock;
use std::time::Duration;

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
            let s = v6.segments();
            let embedded = |hi: u16, lo: u16| Ipv4Addr::new((hi >> 8) as u8, hi as u8, (lo >> 8) as u8, lo as u8);
            // NAT64 (64:ff9b::/96) and IPv4-compatible (::/96) addresses carry an IPv4 address: judged as it.
            if (s[0] == 0x64 && s[1] == 0xff9b && s[2..6] == [0, 0, 0, 0]) || s[..6] == [0; 6] {
                return is_public(IpAddr::V4(embedded(s[6], s[7])));
            }
            // 6to4 (2002::/16) carries one in its next 32 bits.
            if s[0] == 0x2002 {
                return is_public(IpAddr::V4(embedded(s[1], s[2])));
            }
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (s[0] & 0xfe00) == 0xfc00
                || (s[0] & 0xffc0) == 0xfe80
                // Site-local (deprecated), local-use NAT64, discard-only, Teredo, documentation, ORCHID.
                || (s[0] & 0xffc0) == 0xfec0
                || (s[0] == 0x64 && s[1] == 0xff9b)
                || (s[0] == 0x100 && s[1..4] == [0, 0, 0])
                || (s[0] == 0x2001 && s[1] < 0x200)
                || (s[0] == 0x2001 && s[1] == 0x0db8)
                || (s[0] == 0x3fff && s[1] < 0x1000))
        }
    }
}

/// An outbound URL as the HTTP client will read it: http(s), a host, and no
/// user info (`user@host` hides where a request goes from whoever reads it).
pub fn parse(url: &str) -> Result<reqwest::Url, String> {
    let u = reqwest::Url::parse(url.trim()).map_err(|_| format!("{url} isn't a web address."))?;
    if !matches!(u.scheme(), "http" | "https") {
        return Err(format!("{url} isn't an http(s) URL."));
    }
    if !u.username().is_empty() || u.password().is_some() {
        return Err("A web address with a user name or password before the host isn't allowed here.".into());
    }
    if u.host().is_none() {
        return Err(format!("{url} has no host."));
    }
    Ok(u)
}

/// Whether `url` reaches only public addresses (every address its name has),
/// read by the HTTP client's own parser ([`parse`]).
pub async fn check_public(url: &str) -> Result<(), String> {
    check_parsed(&parse(url)?).await
}

/// [`check_public`] for a parsed URL.
pub async fn check_parsed(u: &reqwest::Url) -> Result<(), String> {
    let port = u.port_or_known_default().ok_or_else(|| format!("{u} has no port."))?;
    let host = u.host_str().ok_or_else(|| format!("{u} has no host."))?;
    // The parser writes every IPv4 spelling (2130706433, 0x7f000001, 127.1) dotted, and IPv6 in brackets.
    if let Some(v6) = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')) {
        return public_or(IpAddr::V6(v6.parse().map_err(|_| format!("{host} isn't an address."))?));
    }
    if let Ok(v4) = host.parse::<Ipv4Addr>() {
        return public_or(IpAddr::V4(v4));
    }
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    if host.is_empty() || host == "localhost" || host.ends_with(".localhost") || host.ends_with(".internal") || host.ends_with(".local") {
        return Err(format!("{host} isn't on the public internet."));
    }
    let addrs: Vec<IpAddr> =
        tokio::net::lookup_host((host.as_str(), port)).await.map_err(|e| format!("{host} doesn't resolve: {e}"))?.map(|a| a.ip()).collect();
    if addrs.is_empty() || addrs.iter().any(|ip| !is_public(*ip)) {
        return Err(format!("{host} isn't on the public internet."));
    }
    Ok(())
}

fn public_or(ip: IpAddr) -> Result<(), String> {
    if is_public(ip) { Ok(()) } else { Err(format!("{ip} isn't on the public internet.")) }
}

/// In hosted mode, [`check_public`]; otherwise anything goes.
pub async fn guard_url(url: &str) -> Result<(), String> {
    if !hosted() {
        return Ok(());
    }
    check_public(url).await
}

/// The URL to request, parsed once: in hosted mode it must pass
/// [`check_parsed`]. Send this `Url`, not the string, so what was checked is
/// what is requested.
pub async fn guarded(url: &str) -> Result<reqwest::Url, String> {
    let u = if hosted() { parse(url)? } else { reqwest::Url::parse(url.trim()).map_err(|_| format!("{url} isn't a web address."))? };
    if hosted() {
        check_parsed(&u).await?;
    }
    Ok(u)
}

/// A client builder for requests to URLs a person typed. In hosted mode its
/// names resolve to public addresses only, at connect time (a name pointed
/// at a private address after it was checked isn't reached), and it follows
/// no redirect (a public URL redirecting into the private network).
pub fn client_builder() -> reqwest::ClientBuilder {
    let b = reqwest::Client::builder().connect_timeout(Duration::from_secs(10));
    if hosted() { b.redirect(reqwest::redirect::Policy::none()).dns_resolver(std::sync::Arc::new(PublicOnly)) } else { b }
}

/// Resolves names to their public addresses only ([`is_public`]); none is an error.
pub struct PublicOnly;

impl reqwest::dns::Resolve for PublicOnly {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        Box::pin(async move {
            let host = name.as_str().to_owned();
            let keep: Vec<std::net::SocketAddr> = tokio::net::lookup_host((host.as_str(), 0)).await?.filter(|a| is_public(a.ip())).collect();
            if keep.is_empty() {
                return Err(format!("{host} has no public address").into());
            }
            Ok(Box::new(keep.into_iter()) as reqwest::dns::Addrs)
        })
    }
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
    fn urls_are_read_as_the_client_reads_them() {
        assert_eq!(parse("https://hooks.example.com/x?y=1").unwrap().host_str(), Some("hooks.example.com"));
        assert_eq!(parse("http://10.0.0.5:8080/v1").unwrap().port(), Some(8080));
        assert!(parse("https://user@169.254.169.254/latest").is_err(), "no user info");
        assert!(parse("ftp://x").is_err());
    }

    /// Astra 02 F12: the guard reads the URL as the client does, and private
    /// addresses in every spelling are refused.
    #[tokio::test]
    async fn every_spelling_of_a_private_address_is_refused() {
        for url in [
            "http://127.0.0.1:7997\\@1.1.1.1/",
            "http://2130706433:7997",
            "http://0x7f000001:7997",
            "http://127.1:7997",
            "http://[::ffff:127.0.0.1]:7997",
            "http://[64:ff9b::7f00:1]:7997",
            "http://[fec0::1]:7997",
            "http://[::127.0.0.1]:7997",
            "http://[2002:7f00:1::]:7997",
            "http://user:pw@1.1.1.1/",
        ] {
            assert!(check_public(url).await.is_err(), "{url}");
        }
        assert!(check_public("http://[64:ff9b::101:101]/").await.is_ok(), "NAT64 of a public address");
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
