//! Approved S3 endpoints and the lexical rules for buckets, prefixes, and network addresses.
//!
//! Endpoint parsing deliberately performs no name lookup. The server checks every resolved
//! address again when it connects.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use serde_json::Value;

use crate::{Db, DbError};

/// The setting containing the administrator-approved custom S3 endpoints.
pub const S3_ENDPOINTS: &str = "s3_endpoints";
/// The default monthly list-request budget.
pub const S3_LIST_REQUESTS: &str = "s3_list_requests";
/// The default monthly read-request budget.
pub const S3_READ_REQUESTS: &str = "s3_read_requests";
/// The default monthly downloaded-byte budget (`null` means unlimited).
pub const S3_BYTES_DOWN: &str = "s3_bytes_down";

const DEFAULT_LIST_REQUESTS: u64 = 100_000;
const DEFAULT_READ_REQUESTS: u64 = 1_000_000;
const MIN_REQUESTS: u64 = 1_000;
const MIN_BYTES_DOWN: u64 = 1024 * 1024 * 1024;
const MAX_ENDPOINTS: usize = 20;
const MAX_RANGES: usize = 8;

/// Why an endpoint definition is not safe or well formed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EndpointRefusal {
    Empty,
    NotHttps,
    HasPath,
    HasQuery,
    HasCredentials,
    BadHost,
    AddressLiteral,
    BadPort,
    BadRegion,
    BadName,
    NameTaken,
    TooMany,
    BadRange,
    Reserved,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Network {
    V4(u32, u8),
    V6(u128, u8),
}

/// An IPv4 or IPv6 network and prefix length.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IpRange(Network);

impl IpRange {
    /// Parses a CIDR network, normalising away host bits.
    pub fn parse(text: &str) -> Option<IpRange> {
        let (address, prefix) = text.split_once('/')?;
        if address.is_empty() || prefix.is_empty() || prefix.contains('/') {
            return None;
        }
        let prefix = prefix.parse::<u8>().ok()?;
        match address.parse::<IpAddr>().ok()? {
            IpAddr::V4(ip) if prefix <= 32 => {
                let bits = u32::from(ip);
                Some(IpRange(Network::V4(bits & mask_v4(prefix), prefix)))
            }
            IpAddr::V6(ip) if prefix <= 128 => {
                let bits = u128::from(ip);
                Some(IpRange(Network::V6(bits & mask_v6(prefix), prefix)))
            }
            _ => None,
        }
    }

    /// Whether `ip` belongs to this network.
    pub fn contains(&self, ip: IpAddr) -> bool {
        match (&self.0, ip) {
            (Network::V4(network, prefix), IpAddr::V4(ip)) => {
                u32::from(ip) & mask_v4(*prefix) == *network
            }
            (Network::V6(network, prefix), IpAddr::V6(ip)) => {
                u128::from(ip) & mask_v6(*prefix) == *network
            }
            _ => false,
        }
    }

    /// The canonical CIDR text.
    pub fn display(&self) -> String {
        match self.0 {
            Network::V4(network, prefix) => {
                format!("{}/{prefix}", Ipv4Addr::from(network))
            }
            Network::V6(network, prefix) => {
                format!("{}/{prefix}", Ipv6Addr::from(network))
            }
        }
    }

    fn permitted_width(&self) -> bool {
        matches!(self.0, Network::V4(_, 8..=32) | Network::V6(_, 16..=128))
    }
}

/// One administrator-approved endpoint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct S3Endpoint {
    pub name: String,
    pub url: String,
    pub region: String,
    pub path_style: bool,
    pub allow: Vec<IpRange>,
    host: String,
    port: Option<u16>,
    https: bool,
}

impl S3Endpoint {
    /// Parses a custom endpoint without consulting DNS.
    pub fn parse(
        name: &str,
        url: &str,
        region: &str,
        path_style: bool,
        allow: &[String],
        plain_loopback: bool,
    ) -> Result<S3Endpoint, EndpointRefusal> {
        validate_name(name)?;
        validate_region(region)?;
        if allow.len() > MAX_RANGES {
            return Err(EndpointRefusal::BadRange);
        }
        let allow = allow
            .iter()
            .map(|text| IpRange::parse(text).ok_or(EndpointRefusal::BadRange))
            .collect::<Result<Vec<_>, _>>()?;
        if allow.iter().any(|range| !range.permitted_width()) {
            return Err(EndpointRefusal::BadRange);
        }

        let parsed = parse_url(url)?;
        let legacy_ip = parse_legacy_ipv4(&parsed.host);
        let standard_ip = parsed.host.parse::<IpAddr>().ok();
        let literal = standard_ip.or(legacy_ip.map(IpAddr::V4));

        if !parsed.https {
            let loopback_test = plain_loopback
                && parsed.port.is_some()
                && parsed.host == Ipv4Addr::LOCALHOST.to_string();
            if !loopback_test {
                return Err(EndpointRefusal::NotHttps);
            }
        }
        if let Some(ip) = literal
            && parsed.https
            && !allow.iter().any(|range| range.contains(ip))
        {
            return Err(EndpointRefusal::AddressLiteral);
        }
        if literal.is_none() && looks_like_address_literal(&parsed.host) {
            return Err(EndpointRefusal::AddressLiteral);
        }

        let url = canonical_url(parsed.https, &parsed.host, parsed.port);
        Ok(S3Endpoint {
            name: name.to_string(),
            url,
            region: region.to_string(),
            path_style,
            allow,
            host: parsed.host,
            port: parsed.port,
            https: parsed.https,
        })
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn port(&self) -> Option<u16> {
        self.port
    }

    pub fn https(&self) -> bool {
        self.https
    }
}

struct ParsedUrl {
    https: bool,
    host: String,
    port: Option<u16>,
}

fn parse_url(url: &str) -> Result<ParsedUrl, EndpointRefusal> {
    if url.is_empty() {
        return Err(EndpointRefusal::Empty);
    }
    let (https, rest) = if let Some(rest) = url.strip_prefix("https://") {
        (true, rest)
    } else if let Some(rest) = url.strip_prefix("http://") {
        (false, rest)
    } else {
        return Err(EndpointRefusal::NotHttps);
    };
    if rest.contains(['?', '#']) {
        return Err(EndpointRefusal::HasQuery);
    }
    if rest.contains('@') {
        return Err(EndpointRefusal::HasCredentials);
    }
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    if !path.is_empty() {
        return Err(EndpointRefusal::HasPath);
    }
    if authority.is_empty() {
        return Err(EndpointRefusal::BadHost);
    }

    let (host, port) = if let Some(bracketed) = authority.strip_prefix('[') {
        let Some((host, after)) = bracketed.split_once(']') else {
            return Err(EndpointRefusal::BadHost);
        };
        if host.contains('%') || host.parse::<Ipv6Addr>().is_err() {
            return Err(EndpointRefusal::BadHost);
        }
        let port = parse_port_suffix(after)?;
        (host.to_ascii_lowercase(), port)
    } else {
        if authority.matches(':').count() > 1 {
            return Err(EndpointRefusal::BadHost);
        }
        let (host, port) = match authority.rsplit_once(':') {
            Some((host, port)) => (host, Some(parse_port(port)?)),
            None => (authority, None),
        };
        let host = host.strip_suffix('.').unwrap_or(host).to_ascii_lowercase();
        validate_host(&host)?;
        (host, port)
    };
    Ok(ParsedUrl { https, host, port })
}

fn parse_port_suffix(after: &str) -> Result<Option<u16>, EndpointRefusal> {
    if after.is_empty() {
        Ok(None)
    } else if let Some(port) = after.strip_prefix(':') {
        Ok(Some(parse_port(port)?))
    } else {
        Err(EndpointRefusal::BadHost)
    }
}

fn parse_port(text: &str) -> Result<u16, EndpointRefusal> {
    text.parse::<u16>()
        .ok()
        .filter(|port| *port != 0)
        .ok_or(EndpointRefusal::BadPort)
}

fn validate_host(host: &str) -> Result<(), EndpointRefusal> {
    if host.is_empty() || host.len() > 253 {
        return Err(EndpointRefusal::BadHost);
    }
    for label in host.split('.') {
        if label.is_empty()
            || label.len() > 63
            || label.starts_with('-')
            || label.ends_with('-')
            || !label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        {
            return Err(EndpointRefusal::BadHost);
        }
    }
    Ok(())
}

fn canonical_url(https: bool, host: &str, port: Option<u16>) -> String {
    let scheme = if https { "https" } else { "http" };
    let shown_host = if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_string()
    };
    match port {
        Some(port) => format!("{scheme}://{shown_host}:{port}"),
        None => format!("{scheme}://{shown_host}"),
    }
}

fn validate_name(name: &str) -> Result<(), EndpointRefusal> {
    if name.is_empty()
        || name.len() > 40
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err(EndpointRefusal::BadName);
    }
    if name.starts_with("aws") {
        return Err(EndpointRefusal::Reserved);
    }
    Ok(())
}

fn validate_region(region: &str) -> Result<(), EndpointRefusal> {
    if region.is_empty()
        || region.len() > 40
        || !region
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err(EndpointRefusal::BadRegion);
    }
    Ok(())
}

fn valid_aws_region(region: &str) -> bool {
    let parts = region.split('-').collect::<Vec<_>>();
    let Some(first) = parts.first() else {
        return false;
    };
    let Some(last) = parts.last() else {
        return false;
    };
    parts.len() >= 3
        && first.len() == 2
        && first.bytes().all(|byte| byte.is_ascii_lowercase())
        && last.len() == 1
        && last.bytes().all(|byte| byte.is_ascii_digit())
        && parts
            .get(1..parts.len().saturating_sub(1))
            .is_some_and(|middle| {
                middle.iter().all(|part| {
                    !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_lowercase())
                })
            })
}

/// Builds a built-in AWS regional endpoint named `aws:<region>`.
pub fn aws_endpoint(name: &str) -> Option<S3Endpoint> {
    let region = name.strip_prefix("aws:")?;
    if !valid_aws_region(region) {
        return None;
    }
    let host = format!("s3.{region}.amazonaws.com");
    Some(S3Endpoint {
        name: name.to_string(),
        url: format!("https://{host}"),
        region: region.to_string(),
        path_style: false,
        allow: Vec::new(),
        host,
        port: None,
        https: true,
    })
}

/// Finds a built-in AWS endpoint or a custom approved endpoint by name.
pub fn find_endpoint(name: &str, settings: &S3Settings) -> Option<S3Endpoint> {
    aws_endpoint(name).or_else(|| {
        settings
            .endpoints
            .iter()
            .find(|endpoint| endpoint.name == name)
            .cloned()
    })
}

/// Whether `name` obeys the S3 bucket naming rules ShareCraft supports.
pub fn valid_bucket(name: &str) -> bool {
    let bytes = name.as_bytes();
    (3..=63).contains(&bytes.len())
        && bytes.first().is_some_and(u8::is_ascii_alphanumeric)
        && bytes.last().is_some_and(u8::is_ascii_alphanumeric)
        && bytes.iter().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'-')
        })
        && !name.contains("..")
        && name.parse::<Ipv4Addr>().is_err()
}

/// Cleans a bucket-relative prefix to either empty or a `/`-terminated sequence of segments.
pub fn clean_prefix(text: &str) -> Option<String> {
    if text.is_empty() {
        return Some(String::new());
    }
    if text.starts_with('/') || text.len() > 512 {
        return None;
    }
    let without_slash = text.strip_suffix('/').unwrap_or(text);
    if without_slash.is_empty()
        || without_slash.split('/').any(|part| {
            part.is_empty() || matches!(part, "." | "..") || part.chars().any(char::is_control)
        })
    {
        return None;
    }
    let cleaned = format!("{without_slash}/");
    (cleaned.len() <= 512).then_some(cleaned)
}

/// Whether either clean prefix contains the other by whole path segments.
pub fn prefixes_overlap(a: &str, b: &str) -> bool {
    a.is_empty() || b.is_empty() || a == b || a.starts_with(b) || b.starts_with(a)
}

/// The stable location text stored for an S3-backed library.
pub fn s3_location(endpoint: &str, bucket: &str, prefix: &str) -> String {
    format!("s3://{endpoint}/{bucket}/{prefix}")
}

/// Whether an address is public, or is explicitly included in an administrator-approved range.
pub fn address_allowed(ip: IpAddr, allow: &[IpRange]) -> bool {
    if allow.iter().any(|range| range.contains(ip)) {
        return true;
    }
    match ip {
        IpAddr::V4(ip) => public_v4(ip),
        IpAddr::V6(ip) => public_v6(ip),
    }
}

fn public_v4(ip: Ipv4Addr) -> bool {
    let value = u32::from(ip);
    ![
        ("0.0.0.0", 8),
        ("10.0.0.0", 8),
        ("100.64.0.0", 10),
        ("127.0.0.0", 8),
        ("169.254.0.0", 16),
        ("172.16.0.0", 12),
        ("192.0.0.0", 24),
        ("192.0.2.0", 24),
        ("192.168.0.0", 16),
        ("198.18.0.0", 15),
        ("198.51.100.0", 24),
        ("203.0.113.0", 24),
        ("224.0.0.0", 4),
        ("240.0.0.0", 4),
    ]
    .iter()
    .any(|(network, prefix)| {
        network
            .parse::<Ipv4Addr>()
            .ok()
            .is_some_and(|network| value & mask_v4(*prefix) == u32::from(network))
    })
}

fn public_v6(ip: Ipv6Addr) -> bool {
    let value = u128::from(ip);
    if ip.is_unspecified()
        || ip.is_loopback()
        || in_v6(value, "fc00::", 7)
        || in_v6(value, "fe80::", 10)
        || in_v6(value, "ff00::", 8)
        || in_v6(value, "2001:db8::", 32)
        || in_v6(value, "2001:2::", 48)
        // Teredo, 6to4 and local-use NAT64 tunnel to addresses this check cannot see.
        || in_v6(value, "2001::", 32)
        || in_v6(value, "2002::", 16)
        || in_v6(value, "64:ff9b:1::", 48)
    {
        return false;
    }
    if let Some(mapped) = ip.to_ipv4_mapped() {
        return public_v4(mapped);
    }
    let high_96 = value >> 32;
    let nat64_high_96 = u128::from(Ipv6Addr::new(0x0064, 0xff9b, 0, 0, 0, 0, 0, 0)) >> 32;
    if high_96 == nat64_high_96 || high_96 == 0 {
        return public_v4(Ipv4Addr::from(value as u32));
    }
    true
}

fn in_v6(value: u128, network: &str, prefix: u8) -> bool {
    network
        .parse::<Ipv6Addr>()
        .ok()
        .is_some_and(|network| value & mask_v6(prefix) == u128::from(network))
}

fn mask_v4(prefix: u8) -> u32 {
    u32::MAX.checked_shl(u32::from(32 - prefix)).unwrap_or(0)
}

fn mask_v6(prefix: u8) -> u128 {
    u128::MAX.checked_shl(u32::from(128 - prefix)).unwrap_or(0)
}

fn looks_like_address_literal(host: &str) -> bool {
    host.split('.').all(|part| {
        !part.is_empty()
            && (part.bytes().all(|byte| byte.is_ascii_digit())
                || part.strip_prefix("0x").is_some_and(|digits| {
                    !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_hexdigit())
                }))
    })
}

fn parse_legacy_ipv4(host: &str) -> Option<Ipv4Addr> {
    let parts = host
        .split('.')
        .map(parse_legacy_number)
        .collect::<Option<Vec<_>>>()?;
    let value = match parts.as_slice() {
        [a] if *a <= u64::from(u32::MAX) => *a,
        [a, b] if *a <= 0xff && *b <= 0x00ff_ffff => (a << 24) | b,
        [a, b, c] if *a <= 0xff && *b <= 0xff && *c <= 0xffff => (a << 24) | (b << 16) | c,
        [a, b, c, d] if [a, b, c, d].iter().all(|part| **part <= 0xff) => {
            (a << 24) | (b << 16) | (c << 8) | d
        }
        _ => return None,
    };
    u32::try_from(value).ok().map(Ipv4Addr::from)
}

fn parse_legacy_number(text: &str) -> Option<u64> {
    if let Some(hex) = text.strip_prefix("0x") {
        return u64::from_str_radix(hex, 16).ok();
    }
    if text.len() > 1 && text.starts_with('0') {
        return u64::from_str_radix(text, 8).ok();
    }
    text.parse().ok()
}

/// S3 endpoint approvals and default monthly budgets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct S3Settings {
    pub endpoints: Vec<S3Endpoint>,
    pub list_requests: u64,
    pub read_requests: u64,
    pub bytes_down: Option<u64>,
}

pub(crate) fn valid_endpoints_setting(value: &Value) -> bool {
    endpoint_values(value, true).is_ok()
}

pub(crate) fn valid_request_setting(value: &Value) -> bool {
    value.as_u64().is_some_and(|count| count >= MIN_REQUESTS)
}

pub(crate) fn valid_bytes_setting(value: &Value) -> bool {
    value.is_null() || value.as_u64().is_some_and(|bytes| bytes >= MIN_BYTES_DOWN)
}

fn endpoint_values(value: &Value, strict: bool) -> Result<Vec<S3Endpoint>, EndpointRefusal> {
    let values = value.as_array().ok_or(EndpointRefusal::BadName)?;
    if values.len() > MAX_ENDPOINTS {
        return Err(EndpointRefusal::TooMany);
    }
    let mut endpoints = Vec::new();
    for value in values {
        let Some(object) = value.as_object() else {
            if strict {
                return Err(EndpointRefusal::BadName);
            }
            continue;
        };
        let parsed = (|| {
            let name = object.get("name")?.as_str()?;
            let url = object.get("url")?.as_str()?;
            let region = object.get("region")?.as_str()?;
            let path_style = object.get("path_style")?.as_bool()?;
            let allow = object
                .get("allow")?
                .as_array()?
                .iter()
                .map(|entry| entry.as_str().map(str::to_string))
                .collect::<Option<Vec<_>>>()?;
            Some(S3Endpoint::parse(
                name, url, region, path_style, &allow, false,
            ))
        })();
        let endpoint = match parsed {
            Some(Ok(endpoint)) => endpoint,
            Some(Err(error)) if strict => return Err(error),
            None if strict => return Err(EndpointRefusal::BadName),
            _ => {
                eprintln!("ignored an invalid stored approved S3 endpoint");
                continue;
            }
        };
        if endpoints
            .iter()
            .any(|existing: &S3Endpoint| existing.name == endpoint.name)
        {
            if strict {
                return Err(EndpointRefusal::NameTaken);
            }
            eprintln!("ignored a duplicate stored approved S3 endpoint name");
            continue;
        }
        endpoints.push(endpoint);
    }
    Ok(endpoints)
}

impl Db {
    /// Reads the approved S3 endpoints and budget defaults, safely ignoring malformed stored
    /// values which may predate the current rules.
    pub async fn s3_settings(&self) -> Result<S3Settings, DbError> {
        let rows: Vec<(String, Value)> =
            sqlx::query_as("select key, value from settings where key = any($1) order by key")
                .bind(vec![
                    S3_ENDPOINTS,
                    S3_LIST_REQUESTS,
                    S3_READ_REQUESTS,
                    S3_BYTES_DOWN,
                ])
                .fetch_all(self.pool())
                .await?;
        let mut settings = S3Settings {
            endpoints: Vec::new(),
            list_requests: DEFAULT_LIST_REQUESTS,
            read_requests: DEFAULT_READ_REQUESTS,
            bytes_down: None,
        };
        for (key, value) in rows {
            match key.as_str() {
                S3_ENDPOINTS => {
                    settings.endpoints = endpoint_values(&value, false).unwrap_or_default();
                }
                S3_LIST_REQUESTS => {
                    if let Some(count) = value.as_u64().filter(|count| *count >= MIN_REQUESTS) {
                        settings.list_requests = count;
                    }
                }
                S3_READ_REQUESTS => {
                    if let Some(count) = value.as_u64().filter(|count| *count >= MIN_REQUESTS) {
                        settings.read_requests = count;
                    }
                }
                S3_BYTES_DOWN => {
                    settings.bytes_down = value.as_u64().filter(|bytes| *bytes >= MIN_BYTES_DOWN);
                }
                _ => {}
            }
        }
        Ok(settings)
    }
}
