//! AWS Signature Version 4 for S3 requests.

use std::collections::BTreeMap;
use std::fmt;

use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};
use time::{OffsetDateTime, UtcOffset};

use crate::secrets::Secret;

/// SHA-256 of an empty byte string.
pub const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// The key material used to sign an S3 request.
#[derive(Clone)]
pub struct Credentials {
    /// The public access-key identifier.
    pub access_key_id: String,
    /// The secret access key.
    pub secret: Secret,
    /// Temporary credentials' security token, when present.
    pub session_token: Option<Secret>,
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credentials")
            .field("access_key_id", &self.access_key_id)
            .field("secret", &"[hidden]")
            .field(
                "session_token",
                &self.session_token.as_ref().map(|_| "[hidden]"),
            )
            .finish()
    }
}

/// The parts of an HTTP request covered by its signature.
///
/// `path` is already S3-encoded exactly once. The caller must send the same `host`, payload
/// hash and additional header values supplied here; signing canonicalizes their spelling and
/// whitespace but does not authorize a caller to send different values.
pub struct ToSign<'a> {
    /// HTTP method, such as `GET` or `PUT`.
    pub method: &'a str,
    /// Request authority. HTTPS port 443 is omitted from the canonical value.
    pub host: &'a str,
    /// Canonical, already encoded request path.
    pub path: &'a str,
    /// Unencoded query name/value pairs.
    pub query: &'a [(String, String)],
    /// Additional headers sent with the request.
    pub headers: &'a [(String, String)],
    /// SHA-256 of the request body.
    pub payload_sha256: &'a str,
}

/// Values produced for an authenticated request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Signed {
    /// The complete `Authorization` header value.
    pub authorization: String,
    /// The `x-amz-date` header value.
    pub amz_date: String,
    /// Lower-case hexadecimal request signature.
    pub signature: String,
}

/// Lower-case hexadecimal encoding of `bytes`.
#[must_use]
pub fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        for nibble in [byte >> 4, byte & 0x0f] {
            out.push(char::from(if nibble < 10 {
                b'0' + nibble
            } else {
                b'a' + nibble - 10
            }));
        }
    }
    out
}

/// S3's URI encoding: UTF-8 bytes, upper-case escapes, and optional unescaped slashes.
#[must_use]
pub fn uri_encode(text: &str, keep_slash: bool) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        let unreserved = byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~');
        if unreserved || (keep_slash && byte == b'/') {
            out.push(char::from(byte));
        } else {
            out.push('%');
            for nibble in [byte >> 4, byte & 0x0f] {
                out.push(char::from(if nibble < 10 {
                    b'0' + nibble
                } else {
                    b'A' + nibble - 10
                }));
            }
        }
    }
    out
}

/// Encodes an object key as an absolute S3 request path.
#[must_use]
pub fn key_path(key: &str) -> String {
    format!("/{}", uri_encode(key, true))
}

/// SHA-256 of `bytes`, as lower-case hexadecimal.
#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

fn normalized_value(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn canonical_host(host: &str) -> &str {
    host.strip_suffix(":443").unwrap_or(host)
}

/// Builds the canonical request and its semicolon-separated signed header names.
#[must_use]
pub fn canonical_request(
    r: &ToSign<'_>,
    amz_date: &str,
    session: Option<&Secret>,
) -> (String, String) {
    let mut headers = BTreeMap::new();
    for (name, value) in r.headers {
        headers.insert(name.to_ascii_lowercase(), normalized_value(value));
    }
    headers.insert("host".to_owned(), canonical_host(r.host).to_owned());
    headers.insert(
        "x-amz-content-sha256".to_owned(),
        r.payload_sha256.to_owned(),
    );
    headers.insert("x-amz-date".to_owned(), amz_date.to_owned());
    if let Some(token) = session {
        headers.insert(
            "x-amz-security-token".to_owned(),
            normalized_value(token.expose()),
        );
    }

    let names = headers.keys().cloned().collect::<Vec<_>>().join(";");
    let canonical_headers = headers
        .iter()
        .map(|(name, value)| format!("{name}:{value}\n"))
        .collect::<String>();

    let mut query = r
        .query
        .iter()
        .map(|(name, value)| (uri_encode(name, false), uri_encode(value, false)))
        .collect::<Vec<_>>();
    query.sort();
    let canonical_query = query
        .iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join("&");

    let request = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        r.method, r.path, canonical_query, canonical_headers, names, r.payload_sha256
    );
    (request, names)
}

/// Builds the SigV4 string to sign for a canonical S3 request.
#[must_use]
pub fn string_to_sign(amz_date: &str, region: &str, canonical_request: &str) -> String {
    let date = amz_date.get(..8).unwrap_or(amz_date);
    format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{date}/{region}/s3/aws4_request\n{}",
        sha256_hex(canonical_request.as_bytes())
    )
}

fn hmac(key: &[u8], text: &[u8]) -> [u8; 32] {
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(key) else {
        return [0; 32];
    };
    mac.update(text);
    mac.finalize().into_bytes().into()
}

/// Derives the SigV4 signing key scoped to `date`, `region`, and S3.
#[must_use]
pub fn signing_key(secret: &Secret, date: &str, region: &str) -> [u8; 32] {
    let mut aws_secret = Vec::with_capacity(4 + secret.expose().len());
    aws_secret.extend_from_slice(b"AWS4");
    aws_secret.extend_from_slice(secret.expose().as_bytes());
    let date_key = hmac(&aws_secret, date.as_bytes());
    let region_key = hmac(&date_key, region.as_bytes());
    let service_key = hmac(&region_key, b"s3");
    hmac(&service_key, b"aws4_request")
}

fn stamps(at: OffsetDateTime) -> (String, String) {
    let at = at.to_offset(UtcOffset::UTC);
    let date = format!("{:04}{:02}{:02}", at.year(), u8::from(at.month()), at.day());
    let amz_date = format!(
        "{date}T{:02}{:02}{:02}Z",
        at.hour(),
        at.minute(),
        at.second()
    );
    (date, amz_date)
}

/// Signs `r` for S3 using the credentials and UTC request time supplied.
#[must_use]
pub fn sign(c: &Credentials, region: &str, at: OffsetDateTime, r: &ToSign<'_>) -> Signed {
    let (date, amz_date) = stamps(at);
    let (canonical, signed_headers) = canonical_request(r, &amz_date, c.session_token.as_ref());
    let to_sign = string_to_sign(&amz_date, region, &canonical);
    let key = signing_key(&c.secret, &date, region);
    let signature = hex(&hmac(&key, to_sign.as_bytes()));
    let authorization = format!(
        "AWS4-HMAC-SHA256 Credential={}/{date}/{region}/s3/aws4_request,SignedHeaders={signed_headers},Signature={signature}",
        c.access_key_id
    );
    Signed {
        authorization,
        amz_date,
        signature,
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use time::OffsetDateTime;

    use super::*;
    use crate::secrets::Secret;

    const ACCESS_KEY: &str = "AKIAIOSFODNN7EXAMPLE";
    const SECRET_KEY: &str = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
    const REGION: &str = "us-east-1";
    const HOST: &str = "examplebucket.s3.amazonaws.com";
    const AMZ_DATE: &str = "20130524T000000Z";

    fn credentials() -> Credentials {
        Credentials {
            access_key_id: ACCESS_KEY.to_owned(),
            secret: Secret::new(SECRET_KEY),
            session_token: None,
        }
    }

    fn example_time() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_369_353_600).unwrap()
    }

    fn assert_example(
        request: &ToSign<'_>,
        canonical_hash: &str,
        signature: &str,
        signed_headers: &str,
    ) {
        let (canonical, names) = canonical_request(request, AMZ_DATE, None);
        assert_eq!(sha256_hex(canonical.as_bytes()), canonical_hash);
        assert_eq!(names, signed_headers);

        let signed = sign(&credentials(), REGION, example_time(), request);
        assert_eq!(signed.amz_date, AMZ_DATE);
        assert_eq!(signed.signature, signature);
        assert_eq!(
            signed.authorization,
            format!(
                "AWS4-HMAC-SHA256 Credential={ACCESS_KEY}/20130524/{REGION}/s3/aws4_request,SignedHeaders={signed_headers},Signature={signature}"
            )
        );
    }

    #[test]
    fn aws_example_get_object() {
        let query = [];
        let headers = [("range".to_owned(), "bytes=0-9".to_owned())];
        let request = ToSign {
            method: "GET",
            host: HOST,
            path: "/test.txt",
            query: &query,
            headers: &headers,
            payload_sha256: EMPTY_SHA256,
        };
        assert_example(
            &request,
            "7344ae5b7ee6c3e7e6b0fe0640412a37625d1fbfff95c48bbb2dc43964946972",
            "f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41",
            "host;range;x-amz-content-sha256;x-amz-date",
        );
    }

    #[test]
    fn aws_example_put_object() {
        let query = [];
        let headers = [
            (
                "date".to_owned(),
                "Fri, 24 May 2013 00:00:00 GMT".to_owned(),
            ),
            (
                "x-amz-storage-class".to_owned(),
                "REDUCED_REDUNDANCY".to_owned(),
            ),
        ];
        let request = ToSign {
            method: "PUT",
            host: HOST,
            path: "/test%24file.text",
            query: &query,
            headers: &headers,
            payload_sha256: "44ce7dd67c959e0d3524ffac1771dfbba87d2b6b4b4e99e42034a8b803f8b072",
        };
        assert_example(
            &request,
            "9e0e90d9c76de8fa5b200d8c849cd5b8dc7a3be3951ddb7f6a76b4158342019d",
            "98ad721746da40c64f1a55b78f14c238d841ea1380cd77a1b5971af0ece108bd",
            "date;host;x-amz-content-sha256;x-amz-date;x-amz-storage-class",
        );
    }

    #[test]
    fn aws_example_get_bucket_lifecycle() {
        let query = [("lifecycle".to_owned(), String::new())];
        let headers = [];
        let request = ToSign {
            method: "GET",
            host: HOST,
            path: "/",
            query: &query,
            headers: &headers,
            payload_sha256: EMPTY_SHA256,
        };
        assert_example(
            &request,
            "9766c798316ff2757b517bc739a67f6213b4ab36dd5da2f94eaebf79c77395ca",
            "fea454ca298b7da1c68078a5d1bdbfbbe0d65c699e0f91ac7a200a0136783543",
            "host;x-amz-content-sha256;x-amz-date",
        );
    }

    #[test]
    fn aws_example_list_objects() {
        let query = [
            ("max-keys".to_owned(), "2".to_owned()),
            ("prefix".to_owned(), "J".to_owned()),
        ];
        let headers = [];
        let request = ToSign {
            method: "GET",
            host: HOST,
            path: "/",
            query: &query,
            headers: &headers,
            payload_sha256: EMPTY_SHA256,
        };
        assert_example(
            &request,
            "df57d21db20da04d7fa30298dd4488ba3a2b47ca3a489c74750e0f1e7df1b9b7",
            "34b48302e7b5fa45bde8084f4b7868a86f0a534bc59db6670ed5711ef69dc6f7",
            "host;x-amz-content-sha256;x-amz-date",
        );
    }

    fn canonical_for(
        query: &[(String, String)],
        headers: &[(String, String)],
        host: &str,
    ) -> String {
        let request = ToSign {
            method: "GET",
            host,
            path: "/",
            query,
            headers,
            payload_sha256: EMPTY_SHA256,
        };
        canonical_request(&request, AMZ_DATE, None).0
    }

    #[test]
    fn uri_encode_leaves_unreserved_and_encodes_the_rest_in_upper_case() {
        assert_eq!(uri_encode("AZaz09-._~", false), "AZaz09-._~");
        assert_eq!(uri_encode(" +e\u{301}é", false), "%20%2Be%CC%81%C3%A9");
        assert_eq!(uri_encode("a/b", true), "a/b");
        assert_eq!(uri_encode("a/b", false), "a%2Fb");
    }

    #[test]
    fn query_pairs_are_sorted_after_encoding() {
        let query = [
            ("z".to_owned(), "last".to_owned()),
            ("é".to_owned(), "second".to_owned()),
            ("é".to_owned(), "first".to_owned()),
        ];
        let canonical = canonical_for(&query, &[], HOST);
        assert_eq!(
            canonical.lines().nth(2),
            Some("%C3%A9=first&%C3%A9=second&z=last")
        );
    }

    #[test]
    fn header_values_are_trimmed_and_inner_spaces_collapsed() {
        let headers = [("X-Test".to_owned(), "  many  \t spaces   here  ".to_owned())];
        let canonical = canonical_for(&[], &headers, HOST);
        assert!(canonical.contains("\nx-test:many spaces here\n"));
    }

    #[test]
    fn a_non_default_port_is_in_the_host_and_a_default_one_is_not() {
        let default = canonical_for(&[], &[], "store.example:443");
        let non_default = canonical_for(&[], &[], "store.example:8443");
        assert!(default.contains("\nhost:store.example\n"));
        assert!(!default.contains("store.example:443"));
        assert!(non_default.contains("\nhost:store.example:8443\n"));
    }

    #[test]
    fn a_session_token_is_signed() {
        let token = Secret::new("temporary-token");
        let request = ToSign {
            method: "GET",
            host: HOST,
            path: "/",
            query: &[],
            headers: &[],
            payload_sha256: EMPTY_SHA256,
        };
        let (canonical, names) = canonical_request(&request, AMZ_DATE, Some(&token));
        assert!(canonical.contains("\nx-amz-security-token:temporary-token\n"));
        assert_eq!(
            names,
            "host;x-amz-content-sha256;x-amz-date;x-amz-security-token"
        );
    }

    #[test]
    fn debug_of_credentials_hides_the_secret() {
        let credentials = Credentials {
            access_key_id: "visible-key-id".to_owned(),
            secret: Secret::new("must-never-appear"),
            session_token: Some(Secret::new("nor-may-this")),
        };
        let debug = format!("{credentials:?}");
        assert!(debug.contains("visible-key-id"));
        assert!(debug.contains("[hidden]"));
        assert!(!debug.contains("must-never-appear"));
        assert!(!debug.contains("nor-may-this"));
    }

    #[test]
    fn the_signing_key_changes_with_date_and_region() {
        let secret = Secret::new(SECRET_KEY);
        let original = signing_key(&secret, "20130524", "us-east-1");
        assert_ne!(original, signing_key(&secret, "20130525", "us-east-1"));
        assert_ne!(original, signing_key(&secret, "20130524", "eu-west-1"));
    }
}
