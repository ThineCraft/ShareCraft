//! How the server is reached: development mode, on this computer only, or production mode, at a
//! public HTTPS address behind trusted reverse proxies or a tunnel.
//!
//! In development mode nothing here changes what the server did before production mode
//! existed: loopback names only, `http://` origins, the `sharecraft_session` cookie.
//!
//! In production mode ([`Production`]):
//! - requests are accepted only from the trusted proxies, except `GET /healthz` from loopback;
//! - `X-Forwarded-For`, `X-Forwarded-Proto` and `X-Forwarded-Host` are read only from them;
//! - only requests the proxy says arrived over HTTPS are served;
//! - the origin check compares with the configured public address;
//! - the session cookie is `__Host-sharecraft_session`, `Secure`, and HSTS is sent.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use axum::http::header::{HOST, ORIGIN};
use axum::http::{HeaderMap, HeaderName};

use crate::config::ConfigError;

/// The HSTS `max-age` used when `SHARECRAFT_HSTS_SECONDS` is not set: one year.
pub const DEFAULT_HSTS_SECONDS: u64 = 31_536_000;

/// The narrowest prefix allowed for a trusted IPv4 range: wider ranges would trust too many.
const MIN_V4_PREFIX: u8 = 8;
/// The narrowest prefix allowed for a trusted IPv6 range.
const MIN_V6_PREFIX: u8 = 32;

static X_FORWARDED_FOR: HeaderName = HeaderName::from_static("x-forwarded-for");
static X_FORWARDED_PROTO: HeaderName = HeaderName::from_static("x-forwarded-proto");
static X_FORWARDED_HOST: HeaderName = HeaderName::from_static("x-forwarded-host");

/// How the server is reached.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Deployment {
    /// On this computer only, over plain HTTP at a loopback address.
    #[default]
    Development,
    /// At a public HTTPS address, behind trusted proxies.
    Production(Production),
}

/// The settings of production mode.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Production {
    /// The one public address, for example `https://share.example.com`.
    pub public: PublicUrl,
    /// The addresses whose forwarded headers are believed.
    pub proxies: TrustedProxies,
    /// The HSTS `max-age`, in seconds; 0 withdraws it.
    pub hsts_seconds: u64,
}

impl Deployment {
    /// Production mode from the raw settings, or development mode when `public_url` is unset.
    ///
    /// # Errors
    /// `PublicUrlNotHttps`, `BadPublicUrl`, `BadTrustedProxies`, `BadHstsSeconds`, or
    /// `ProxiesWithoutPublicUrl` when proxies are named without a public address.
    pub fn from_values(
        public_url: Option<&str>,
        trusted_proxies: Option<&str>,
        hsts_seconds: Option<&str>,
    ) -> Result<Self, ConfigError> {
        let Some(public_url) = public_url.map(str::trim).filter(|s| !s.is_empty()) else {
            if trusted_proxies.is_some_and(|p| !p.trim().is_empty()) {
                return Err(ConfigError::ProxiesWithoutPublicUrl);
            }
            return Ok(Deployment::Development);
        };
        let public = PublicUrl::parse(public_url)?;
        let proxies = match trusted_proxies.map(str::trim).filter(|s| !s.is_empty()) {
            None => TrustedProxies::local(),
            Some(list) => TrustedProxies::parse(list)?,
        };
        let hsts_seconds = match hsts_seconds.map(str::trim) {
            None | Some("") => DEFAULT_HSTS_SECONDS,
            Some(text) => text
                .parse::<u64>()
                .map_err(|_| ConfigError::BadHstsSeconds(text.to_owned()))?,
        };
        Ok(Deployment::Production(Production {
            public,
            proxies,
            hsts_seconds,
        }))
    }

    /// Production mode at `public`, trusting `proxies`, with the default HSTS time, for tests.
    ///
    /// # Errors
    /// As [`Deployment::from_values`].
    #[doc(hidden)]
    pub fn production_for_tests(public: &str, proxies: &str) -> Result<Self, ConfigError> {
        Deployment::from_values(Some(public), Some(proxies), None)
    }

    /// The production settings, if this is production mode.
    #[must_use]
    pub fn production(&self) -> Option<&Production> {
        match self {
            Deployment::Development => None,
            Deployment::Production(production) => Some(production),
        }
    }

    /// Whether this is production mode.
    #[must_use]
    pub fn is_production(&self) -> bool {
        matches!(self, Deployment::Production(_))
    }

    /// The name of the session cookie.
    #[must_use]
    pub fn cookie_name(&self) -> &'static str {
        match self {
            Deployment::Development => "sharecraft_session",
            Deployment::Production(_) => "__Host-sharecraft_session",
        }
    }

    /// The attributes after the cookie's value, before `Max-Age`.
    #[must_use]
    pub fn cookie_attributes(&self) -> &'static str {
        match self {
            Deployment::Development => "HttpOnly; SameSite=Lax; Path=/",
            Deployment::Production(_) => "Secure; HttpOnly; SameSite=Lax; Path=/",
        }
    }

    /// The `Strict-Transport-Security` value, in production mode.
    #[must_use]
    pub fn hsts_value(&self) -> Option<String> {
        self.production()
            .map(|p| format!("max-age={}", p.hsts_seconds))
    }

    /// The origin pages are served from, as links should name it: the public address in
    /// production, `http://<host>` in development.
    #[must_use]
    pub fn link_origin(&self, host: &str) -> String {
        match self {
            Deployment::Development => format!("http://{host}"),
            Deployment::Production(p) => p.public.origin().to_owned(),
        }
    }

    /// Whether the request comes from a page this server served, at an address it answers to.
    ///
    /// Development: the `Host` is a loopback name and `Origin`, when present, is
    /// `http://<Host>`. Production: the request came through a trusted proxy over HTTPS, its
    /// host is the public address's, and `Origin`, when present, is the public origin.
    #[must_use]
    pub fn same_origin(&self, headers: &HeaderMap, source: &Source) -> bool {
        match self {
            Deployment::Development => development_same_origin(headers),
            Deployment::Production(p) => {
                if !source.via_proxy || !source.https {
                    return false;
                }
                if !source.host.as_deref().is_some_and(|h| p.public.is_host(h)) {
                    return false;
                }
                match headers.get(ORIGIN) {
                    None => true,
                    Some(origin) => origin
                        .to_str()
                        .is_ok_and(|o| o.eq_ignore_ascii_case(p.public.origin())),
                }
            }
        }
    }

    /// Where a request with `headers` came from, over a connection from `peer` (`None` when it
    /// did not come over a connection, as in tests that call the router directly).
    ///
    /// # Errors
    /// In production mode, [`Refusal::Untrusted`] for a request that did not come from a
    /// trusted proxy, except `GET /healthz` from loopback.
    pub fn source_of(
        &self,
        peer: Option<SocketAddr>,
        headers: &HeaderMap,
        health_check: bool,
    ) -> Result<Source, Refusal> {
        match self {
            Deployment::Development => {
                let ip = peer.map_or(IpAddr::V4(Ipv4Addr::LOCALHOST), |p| canonical(p.ip()));
                Ok(Source {
                    client_ip: ip,
                    https: false,
                    via_proxy: false,
                    host: header_text(headers, &HOST).map(str::to_owned),
                })
            }
            Deployment::Production(p) => {
                let Some(peer) = peer.map(|p| canonical(p.ip())) else {
                    return Err(Refusal::Untrusted);
                };
                if !p.proxies.contains(peer) {
                    if health_check && peer.is_loopback() {
                        return Ok(Source {
                            client_ip: peer,
                            https: false,
                            via_proxy: false,
                            host: None,
                        });
                    }
                    return Err(Refusal::Untrusted);
                }
                let client_ip = client_ip(headers, peer, &p.proxies);
                let https = last_value(headers, &X_FORWARDED_PROTO)
                    .is_some_and(|proto| proto.eq_ignore_ascii_case("https"));
                let host = last_value(headers, &X_FORWARDED_HOST)
                    .or_else(|| header_text(headers, &HOST))
                    .map(str::to_owned);
                Ok(Source {
                    client_ip,
                    https,
                    via_proxy: true,
                    host,
                })
            }
        }
    }
}

/// Where a request came from, worked out once by the `source` middleware.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Source {
    /// The client's address: the peer in development, from `X-Forwarded-For` behind proxies.
    pub client_ip: IpAddr,
    /// Whether the request arrived at the proxy over HTTPS. Always false in development.
    pub https: bool,
    /// Whether it came through a trusted proxy.
    pub via_proxy: bool,
    /// The host the client asked for: `X-Forwarded-Host` from a proxy, else `Host`.
    pub host: Option<String>,
}

impl Source {
    /// A source that passes no production check, for a request the middleware never saw.
    #[must_use]
    pub fn unknown() -> Self {
        Source {
            client_ip: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            https: false,
            via_proxy: false,
            host: None,
        }
    }
}

/// The origin check's verdict on a request, worked out once by the `source` middleware and read
/// by the API, WebSocket and editor guards.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SameOrigin(pub bool);

impl SameOrigin {
    /// The verdict in `extensions`; false for a request the middleware never saw.
    #[must_use]
    pub fn of(extensions: &axum::http::Extensions) -> bool {
        extensions.get::<SameOrigin>().is_some_and(|s| s.0)
    }
}

/// Why a request is refused before it reaches a route.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// It did not come from a trusted proxy.
    Untrusted,
}

/// The one public address of a production installation.
#[derive(Clone, PartialEq, Eq)]
pub struct PublicUrl {
    /// The host, lowercase, with `:port` when the port is not 443.
    host: String,
    /// `https://<host>`.
    origin: String,
}

impl fmt::Debug for PublicUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.origin)
    }
}

impl fmt::Display for PublicUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.origin)
    }
}

impl PublicUrl {
    /// Parses `https://host[:port][/]`.
    ///
    /// # Errors
    /// `PublicUrlNotHttps` unless it starts with `https://`; `BadPublicUrl` for a path, query,
    /// fragment, user name or password, an empty or non-lowercase-ASCII host, or a bad port.
    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        let bad = || ConfigError::BadPublicUrl(text.to_owned());
        let rest = match text.get(..8) {
            Some(scheme) if scheme.eq_ignore_ascii_case("https://") => &text[8..],
            _ => return Err(ConfigError::PublicUrlNotHttps(text.to_owned())),
        };
        let rest = rest.strip_suffix('/').unwrap_or(rest);
        if rest.is_empty()
            || rest
                .chars()
                .any(|c| matches!(c, '/' | '?' | '#' | '@' | '\\') || c.is_whitespace())
        {
            return Err(bad());
        }
        let (host, port) = if let Some(inside) = rest.strip_prefix('[') {
            let (addr, after) = inside.split_once(']').ok_or_else(bad)?;
            addr.parse::<Ipv6Addr>().map_err(|_| bad())?;
            if addr.chars().any(|c| c.is_ascii_uppercase()) {
                return Err(bad());
            }
            let port = match after {
                "" => None,
                _ => Some(after.strip_prefix(':').ok_or_else(bad)?),
            };
            (format!("[{addr}]"), port)
        } else {
            let (host, port) = match rest.split_once(':') {
                Some((host, port)) => (host, Some(port)),
                None => (rest, None),
            };
            let valid = !host.is_empty()
                && host
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '-')
                && !host.starts_with(['.', '-'])
                && !host.ends_with('-')
                && !host.contains("..");
            if !valid {
                return Err(bad());
            }
            (host.to_owned(), port)
        };
        let port = match port {
            None => 443,
            Some(port) => match port.parse::<u16>() {
                Ok(p) if p > 0 && !port.starts_with('0') => p,
                _ => return Err(bad()),
            },
        };
        let host = if port == 443 {
            host
        } else {
            format!("{host}:{port}")
        };
        let origin = format!("https://{host}");
        Ok(PublicUrl { host, origin })
    }

    /// `https://<host>[:port]`.
    #[must_use]
    pub fn origin(&self) -> &str {
        &self.origin
    }

    /// The host, with `:port` when it is not 443.
    #[must_use]
    pub fn host(&self) -> &str {
        &self.host
    }

    /// Whether `host`, as a `Host` header gives it, names this address: equal ignoring case,
    /// with an explicit `:443` the same as none.
    #[must_use]
    pub fn is_host(&self, host: &str) -> bool {
        let host = host.strip_suffix(":443").unwrap_or(host);
        host.eq_ignore_ascii_case(&self.host)
    }
}

/// The addresses and ranges whose forwarded headers are believed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrustedProxies(Vec<IpRange>);

impl TrustedProxies {
    /// A proxy or tunnel on this computer: `127.0.0.1` and `::1`.
    #[must_use]
    pub fn local() -> Self {
        TrustedProxies(vec![
            IpRange::single(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            IpRange::single(IpAddr::V6(Ipv6Addr::LOCALHOST)),
        ])
    }

    /// Parses a comma-separated list of addresses and ranges.
    ///
    /// # Errors
    /// `BadTrustedProxies` for an entry that is not an address or range, a range wider than
    /// `/8` (IPv4) or `/32` (IPv6), or an empty list.
    pub fn parse(list: &str) -> Result<Self, ConfigError> {
        let ranges = list
            .split(',')
            .map(str::trim)
            .filter(|entry| !entry.is_empty())
            .map(|entry| {
                IpRange::parse(entry)
                    .ok_or_else(|| ConfigError::BadTrustedProxies(entry.to_owned()))
            })
            .collect::<Result<Vec<_>, _>>()?;
        if ranges.is_empty() {
            return Err(ConfigError::BadTrustedProxies(list.to_owned()));
        }
        Ok(TrustedProxies(ranges))
    }

    /// Whether `ip` is one of the trusted proxies.
    #[must_use]
    pub fn contains(&self, ip: IpAddr) -> bool {
        let ip = canonical(ip);
        self.0.iter().any(|range| range.contains(ip))
    }
}

impl fmt::Display for TrustedProxies {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, range) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str(", ")?;
            }
            write!(f, "{range}")?;
        }
        Ok(())
    }
}

/// An address, or a range of them written `address/prefix`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IpRange {
    base: IpAddr,
    prefix: u8,
}

impl fmt::Display for IpRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let full = if self.base.is_ipv4() { 32 } else { 128 };
        if self.prefix == full {
            write!(f, "{}", self.base)
        } else {
            write!(f, "{}/{}", self.base, self.prefix)
        }
    }
}

impl IpRange {
    fn single(ip: IpAddr) -> Self {
        let prefix = if ip.is_ipv4() { 32 } else { 128 };
        IpRange { base: ip, prefix }
    }

    /// Parses `1.2.3.4`, `1.2.3.0/24`, `::1` or `fd00::/48`; `None` for anything else, or a
    /// range wider than allowed.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let (addr, prefix) = match text.split_once('/') {
            Some((addr, prefix)) => (addr, Some(prefix)),
            None => (text, None),
        };
        let base = canonical(addr.parse::<IpAddr>().ok()?);
        let (full, min) = if base.is_ipv4() {
            (32, MIN_V4_PREFIX)
        } else {
            (128, MIN_V6_PREFIX)
        };
        let prefix = match prefix {
            None => full,
            Some(p) if !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) => p.parse().ok()?,
            Some(_) => return None,
        };
        if prefix > full || prefix < min {
            return None;
        }
        Some(IpRange { base, prefix })
    }

    /// Whether `ip` is in the range.
    #[must_use]
    pub fn contains(&self, ip: IpAddr) -> bool {
        match (self.base, canonical(ip)) {
            (IpAddr::V4(base), IpAddr::V4(ip)) => {
                let mask = u32::MAX
                    .checked_shl(32 - u32::from(self.prefix))
                    .unwrap_or(0);
                u32::from(base) & mask == u32::from(ip) & mask
            }
            (IpAddr::V6(base), IpAddr::V6(ip)) => {
                let mask = u128::MAX
                    .checked_shl(128 - u32::from(self.prefix))
                    .unwrap_or(0);
                u128::from(base) & mask == u128::from(ip) & mask
            }
            _ => false,
        }
    }
}

/// `ip`, with an IPv4-mapped IPv6 address (`::ffff:1.2.3.4`) as its IPv4 address.
#[must_use]
pub fn canonical(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
        IpAddr::V4(_) => ip,
    }
}

/// The client behind `peer`: `X-Forwarded-For` read from the right, skipping trusted proxies.
/// The first entry that is not a trusted proxy is the client. If every entry is trusted, the
/// leftmost is; if the header is missing, or an entry before the client cannot be read, the
/// last address known is.
fn client_ip(headers: &HeaderMap, peer: IpAddr, proxies: &TrustedProxies) -> IpAddr {
    let mut client = peer;
    let entries: Vec<&str> = headers
        .get_all(&X_FORWARDED_FOR)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(str::trim)
        .collect();
    for entry in entries.iter().rev() {
        let Some(ip) = parse_forwarded_ip(entry) else {
            break;
        };
        client = ip;
        if !proxies.contains(ip) {
            break;
        }
    }
    client
}

/// An address as proxies write it in `X-Forwarded-For`: `1.2.3.4`, `1.2.3.4:5678`, `::1` or
/// `[::1]:5678`.
fn parse_forwarded_ip(entry: &str) -> Option<IpAddr> {
    if let Ok(ip) = entry.parse::<IpAddr>() {
        return Some(canonical(ip));
    }
    if let Ok(addr) = entry.parse::<SocketAddr>() {
        return Some(canonical(addr.ip()));
    }
    entry
        .strip_prefix('[')
        .and_then(|e| e.strip_suffix(']'))
        .and_then(|e| e.parse::<IpAddr>().ok())
        .map(canonical)
}

/// The last value of a header that may be repeated or hold a comma-separated list: the one the
/// nearest proxy wrote.
fn last_value<'a>(headers: &'a HeaderMap, name: &HeaderName) -> Option<&'a str> {
    headers
        .get_all(name)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(str::trim)
        .rfind(|v| !v.is_empty())
}

fn header_text<'a>(headers: &'a HeaderMap, name: &HeaderName) -> Option<&'a str> {
    headers.get(name).and_then(|h| h.to_str().ok())
}

/// The development check: a loopback `Host` and, if `Origin` is present, `http://<Host>`.
fn development_same_origin(headers: &HeaderMap) -> bool {
    let Some(host) = header_text(headers, &HOST) else {
        return false;
    };
    if !host_part(host).is_some_and(is_loopback_host) {
        return false;
    }
    match headers.get(ORIGIN) {
        None => true,
        Some(origin) => origin
            .to_str()
            .is_ok_and(|origin| origin.eq_ignore_ascii_case(&format!("http://{host}"))),
    }
}

/// The host of a `Host` header value (`host`, `host:port`, `[v6]` or `[v6]:port`).
pub(crate) fn host_part(value: &str) -> Option<&str> {
    let (host, port) = match value.strip_prefix('[') {
        Some(rest) => {
            let (host, after) = rest.split_once(']')?;
            match after {
                "" => (host, None),
                _ => (host, Some(after.strip_prefix(':')?)),
            }
        }
        None => match value.split_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (value, None),
        },
    };
    if port.is_some_and(|p| p.parse::<u16>().is_err()) || host.is_empty() {
        return None;
    }
    Some(host)
}

pub(crate) fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use axum::http::HeaderValue;

    use super::*;

    #[test]
    fn public_url_parsing() {
        for (text, origin, host) in [
            (
                "https://share.example.com",
                "https://share.example.com",
                "share.example.com",
            ),
            (
                "https://share.example.com/",
                "https://share.example.com",
                "share.example.com",
            ),
            (
                "HTTPS://share.example.com",
                "https://share.example.com",
                "share.example.com",
            ),
            (
                "https://share.example.com:443",
                "https://share.example.com",
                "share.example.com",
            ),
            (
                "https://sharecraft.localhost:8443",
                "https://sharecraft.localhost:8443",
                "sharecraft.localhost:8443",
            ),
            ("https://10.0.0.5", "https://10.0.0.5", "10.0.0.5"),
            (
                "https://[fd00::1]:8443",
                "https://[fd00::1]:8443",
                "[fd00::1]:8443",
            ),
            (
                "https://xn--bcher-kva.example",
                "https://xn--bcher-kva.example",
                "xn--bcher-kva.example",
            ),
        ] {
            let url = PublicUrl::parse(text).unwrap();
            assert_eq!(url.origin(), origin, "{text}");
            assert_eq!(url.host(), host, "{text}");
        }
        for text in [
            "http://share.example.com",
            "share.example.com",
            "ftp://x",
            "",
            "https:/x",
        ] {
            assert_eq!(
                PublicUrl::parse(text),
                Err(ConfigError::PublicUrlNotHttps(text.to_owned())),
                "{text}"
            );
        }
        for text in [
            "https://",
            "https:///",
            "https://share.example.com/share",
            "https://share.example.com/share/",
            "https://share.example.com?x=1",
            "https://share.example.com#top",
            "https://ann@share.example.com",
            "https://ann:pw@share.example.com",
            "https://Share.Example.com",
            "https://bücher.example",
            "https://share example.com",
            "https://share.example.com:0",
            "https://share.example.com:99999",
            "https://share.example.com:",
            "https://share.example.com:08443",
            "https://.example.com",
            "https://share..example.com",
            "https://[fd00::1",
            "https://[FD00::1]",
            "https://share.example.com\\x",
        ] {
            assert_eq!(
                PublicUrl::parse(text),
                Err(ConfigError::BadPublicUrl(text.to_owned())),
                "{text}"
            );
        }
    }

    #[test]
    fn public_host_comparison() {
        let url = PublicUrl::parse("https://share.example.com").unwrap();
        assert!(url.is_host("share.example.com"));
        assert!(url.is_host("SHARE.example.com"));
        assert!(url.is_host("share.example.com:443"));
        assert!(!url.is_host("share.example.com:8443"));
        assert!(!url.is_host("evil.example"));
        let port = PublicUrl::parse("https://share.example.com:8443").unwrap();
        assert!(port.is_host("share.example.com:8443"));
        assert!(!port.is_host("share.example.com"));
    }

    #[test]
    fn trusted_proxy_ranges() {
        let proxies = TrustedProxies::parse(" 127.0.0.1, 10.0.0.0/24 ,fd00::/48 ").unwrap();
        assert!(proxies.contains("127.0.0.1".parse().unwrap()));
        assert!(proxies.contains("10.0.0.200".parse().unwrap()));
        assert!(!proxies.contains("10.0.1.1".parse().unwrap()));
        assert!(proxies.contains("fd00::1".parse().unwrap()));
        assert!(proxies.contains("fd00:0:0:1::5".parse().unwrap()));
        assert!(!proxies.contains("fd00:0:1::1".parse().unwrap()));
        assert!(proxies.contains("::ffff:10.0.0.7".parse().unwrap()));
        assert!(!proxies.contains("127.0.0.2".parse().unwrap()));
        assert_eq!(proxies.to_string(), "127.0.0.1, 10.0.0.0/24, fd00::/48");

        let local = TrustedProxies::local();
        assert!(local.contains("127.0.0.1".parse().unwrap()));
        assert!(local.contains("::1".parse().unwrap()));
        assert!(local.contains("::ffff:127.0.0.1".parse().unwrap()));
        assert!(!local.contains("192.168.1.2".parse().unwrap()));

        assert!(TrustedProxies::parse("10.0.0.0/8").is_ok());
        assert!(TrustedProxies::parse("2001:db8::/32").is_ok());
        for bad in [
            "0.0.0.0/0",
            "::/0",
            "10.0.0.0/7",
            "2001:db8::/31",
            "10.0.0.0/33",
            "fd00::/129",
            "localhost",
            "10.0.0.0/",
            "10.0.0.0/+8",
            "1.2.3",
            ",",
            "",
        ] {
            assert!(TrustedProxies::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn deployment_from_values() {
        assert_eq!(
            Deployment::from_values(None, None, None),
            Ok(Deployment::Development)
        );
        assert_eq!(
            Deployment::from_values(Some("  "), Some(""), None),
            Ok(Deployment::Development)
        );
        assert_eq!(
            Deployment::from_values(None, Some("127.0.0.1"), None),
            Err(ConfigError::ProxiesWithoutPublicUrl)
        );
        let prod = Deployment::from_values(Some("https://share.example.com"), None, None).unwrap();
        let p = prod.production().unwrap();
        assert_eq!(p.proxies, TrustedProxies::local());
        assert_eq!(p.hsts_seconds, DEFAULT_HSTS_SECONDS);
        assert_eq!(prod.hsts_value().as_deref(), Some("max-age=31536000"));
        let short = Deployment::from_values(Some("https://share.example.com"), None, Some(" 300 "))
            .unwrap();
        assert_eq!(short.hsts_value().as_deref(), Some("max-age=300"));
        let off =
            Deployment::from_values(Some("https://share.example.com"), None, Some("0")).unwrap();
        assert_eq!(off.hsts_value().as_deref(), Some("max-age=0"));
        for bad in ["-1", "soon", "1.5"] {
            assert_eq!(
                Deployment::from_values(Some("https://share.example.com"), None, Some(bad)),
                Err(ConfigError::BadHstsSeconds(bad.to_owned()))
            );
        }
        assert_eq!(Deployment::Development.hsts_value(), None);
    }

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.append(
                HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        map
    }

    fn peer(ip: &str) -> Option<SocketAddr> {
        Some(SocketAddr::new(ip.parse().unwrap(), 50_000))
    }

    fn production() -> Deployment {
        Deployment::production_for_tests("https://share.example.com", "127.0.0.1, 10.0.0.0/24")
            .unwrap()
    }

    #[test]
    fn forwarded_headers_only_from_trusted_proxies() {
        let d = production();
        let forged = headers(&[
            ("host", "share.example.com"),
            ("x-forwarded-for", "1.2.3.4"),
            ("x-forwarded-proto", "https"),
            ("x-forwarded-host", "share.example.com"),
        ]);
        // An untrusted peer is refused outright, whatever it sends.
        assert_eq!(
            d.source_of(peer("203.0.113.9"), &forged, false),
            Err(Refusal::Untrusted)
        );
        assert_eq!(d.source_of(None, &forged, false), Err(Refusal::Untrusted));
        // ...except the health check from loopback, which is not believed either.
        let health = d.source_of(peer("127.0.0.5"), &forged, true).unwrap();
        assert_eq!(health.client_ip, "127.0.0.5".parse::<IpAddr>().unwrap());
        assert!(!health.https && !health.via_proxy && health.host.is_none());
        assert_eq!(
            d.source_of(peer("203.0.113.9"), &forged, true),
            Err(Refusal::Untrusted)
        );

        // A client adding its own entry before the proxy's cannot choose its address.
        let spoof = headers(&[
            ("host", "share.example.com"),
            ("x-forwarded-for", "1.2.3.4, 198.51.100.7"),
            ("x-forwarded-proto", "https"),
        ]);
        let s = d.source_of(peer("127.0.0.1"), &spoof, false).unwrap();
        assert_eq!(s.client_ip, "198.51.100.7".parse::<IpAddr>().unwrap());
        assert!(s.https && s.via_proxy);
        assert_eq!(s.host.as_deref(), Some("share.example.com"));

        // Two trusted proxies in a chain, in two headers.
        let chain = headers(&[
            ("x-forwarded-for", "198.51.100.7"),
            ("x-forwarded-for", "10.0.0.3"),
            ("x-forwarded-proto", "http, https"),
        ]);
        let s = d.source_of(peer("127.0.0.1"), &chain, false).unwrap();
        assert_eq!(s.client_ip, "198.51.100.7".parse::<IpAddr>().unwrap());
        assert!(s.https);

        // Every entry trusted: the leftmost.
        let inner = headers(&[("x-forwarded-for", "10.0.0.9, 10.0.0.3")]);
        let s = d.source_of(peer("127.0.0.1"), &inner, false).unwrap();
        assert_eq!(s.client_ip, "10.0.0.9".parse::<IpAddr>().unwrap());
        assert!(!s.https);

        // Ports and brackets as proxies write them; mapped addresses as IPv4.
        for (entry, expect) in [
            ("198.51.100.7:4444", "198.51.100.7"),
            ("[2001:db8::7]:4444", "2001:db8::7"),
            ("[2001:db8::7]", "2001:db8::7"),
            ("::ffff:198.51.100.7", "198.51.100.7"),
        ] {
            let h = headers(&[("x-forwarded-for", entry)]);
            let s = d.source_of(peer("127.0.0.1"), &h, false).unwrap();
            assert_eq!(s.client_ip, expect.parse::<IpAddr>().unwrap(), "{entry}");
        }

        // Missing, empty or unreadable: the last address known.
        for value in ["", "garbage", "1.2.3.4, garbage"] {
            let h = headers(&[("x-forwarded-for", value)]);
            let s = d.source_of(peer("127.0.0.1"), &h, false).unwrap();
            assert_eq!(
                s.client_ip,
                "127.0.0.1".parse::<IpAddr>().unwrap(),
                "{value:?}"
            );
        }
        let s = d
            .source_of(peer("127.0.0.1"), &HeaderMap::new(), false)
            .unwrap();
        assert_eq!(s.client_ip, "127.0.0.1".parse::<IpAddr>().unwrap());
        assert!(!s.https);
        assert_eq!(s.host, None);

        // The last X-Forwarded-Host wins over Host.
        let hosts = headers(&[
            ("host", "127.0.0.1:8787"),
            ("x-forwarded-host", "evil.example, share.example.com"),
        ]);
        let s = d.source_of(peer("127.0.0.1"), &hosts, false).unwrap();
        assert_eq!(s.host.as_deref(), Some("share.example.com"));

        // Development believes nothing forwarded.
        let s = Deployment::Development
            .source_of(peer("127.0.0.1"), &forged, false)
            .unwrap();
        assert_eq!(s.client_ip, "127.0.0.1".parse::<IpAddr>().unwrap());
        assert!(!s.https && !s.via_proxy);
    }

    fn proxied(d: &Deployment, pairs: &[(&str, &str)]) -> bool {
        let h = headers(pairs);
        let source = d.source_of(peer("127.0.0.1"), &h, false).unwrap();
        d.same_origin(&h, &source)
    }

    #[test]
    fn public_origin_check() {
        let d = production();
        let https = ("x-forwarded-proto", "https");
        let host = ("host", "share.example.com");
        assert!(proxied(&d, &[host, https]));
        assert!(proxied(
            &d,
            &[host, https, ("origin", "https://share.example.com")]
        ));
        assert!(proxied(
            &d,
            &[host, https, ("origin", "HTTPS://Share.Example.com")]
        ));
        assert!(proxied(
            &d,
            &[
                ("host", "127.0.0.1:8787"),
                ("x-forwarded-host", "share.example.com"),
                https
            ]
        ));
        for origin in [
            "https://evil.example",
            "http://share.example.com",
            "https://share.example.com:8443",
            "https://share.example.com.evil.example",
            "null",
        ] {
            assert!(!proxied(&d, &[host, https, ("origin", origin)]), "{origin}");
        }
        // DNS rebinding and wrong names.
        assert!(!proxied(&d, &[("host", "evil.example"), https]));
        assert!(!proxied(&d, &[("host", "127.0.0.1:8787"), https]));
        assert!(!proxied(&d, &[https]));
        // Plain HTTP at the proxy.
        assert!(!proxied(&d, &[host]));
        assert!(!proxied(&d, &[host, ("x-forwarded-proto", "http")]));
        // A source the middleware never made passes nothing.
        assert!(!d.same_origin(&headers(&[host]), &Source::unknown()));
    }

    #[test]
    fn development_mode_unchanged() {
        let d = Deployment::Development;
        let check = |host: Option<&str>, origin: Option<&str>| {
            let mut pairs = Vec::new();
            if let Some(h) = host {
                pairs.push(("host", h));
            }
            if let Some(o) = origin {
                pairs.push(("origin", o));
            }
            let h = headers(&pairs);
            let source = d.source_of(peer("127.0.0.1"), &h, false).unwrap();
            d.same_origin(&h, &source)
        };
        assert!(check(Some("127.0.0.1:8787"), None));
        assert!(check(Some("127.0.0.1:8787"), Some("http://127.0.0.1:8787")));
        assert!(check(Some("localhost:8787"), Some("http://LOCALHOST:8787")));
        assert!(check(Some("[::1]:8787"), Some("http://[::1]:8787")));
        assert!(!check(None, None));
        assert!(!check(Some("evil.example"), None));
        assert!(!check(Some("127.0.0.1:8787"), Some("http://evil.example")));
        assert!(!check(
            Some("127.0.0.1:8787"),
            Some("http://127.0.0.1:9999")
        ));
        assert!(!check(
            Some("127.0.0.1:8787"),
            Some("https://127.0.0.1:8787")
        ));
        assert_eq!(d.cookie_name(), "sharecraft_session");
        assert_eq!(d.link_origin("127.0.0.1:8787"), "http://127.0.0.1:8787");
        assert_eq!(
            production().link_origin("evil.example"),
            "https://share.example.com"
        );
        assert_eq!(production().cookie_name(), "__Host-sharecraft_session");
    }
}
