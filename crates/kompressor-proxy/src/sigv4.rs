//! AWS Signature Version 4, for Bedrock requests this proxy rewrote.
//!
//! A SigV4 signature covers the body and the host. A client that signed its
//! request for `127.0.0.1` and a body we then compressed has a signature AWS
//! will refuse twice over, so an upstream configured with `aws_sigv4` has the
//! client's own signing headers taken off and the request signed here, over
//! the bytes that actually go out — on every send, so a retry or a retrieve
//! round is signed for the body it carries.
//!
//! Credentials are the standard variables: `AWS_ACCESS_KEY_ID`,
//! `AWS_SECRET_ACCESS_KEY`, and `AWS_SESSION_TOKEN` when present. Profiles,
//! SSO and instance roles are not read; a deployment on one of those exports
//! the variables (`aws configure export-credentials --format env`). A client
//! that authenticates with a Bedrock API key (`Authorization: Bearer …`) is
//! not signed for: that key does not cover the body, and it is forwarded.
//!
//! Implemented on `ring` (HMAC-SHA256, SHA-256), which the workspace already
//! links, rather than on the AWS SDK. Checked against AWS's published example.

use http::{HeaderMap, HeaderName, HeaderValue};
use ring::{digest, hmac};

/// Credentials to sign with.
#[derive(Clone)]
pub struct Credentials {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("access_key_id", &self.access_key_id)
            .field("secret_access_key", &"***")
            .field("session_token", &self.session_token.as_ref().map(|_| "***"))
            .finish()
    }
}

impl Credentials {
    /// From the standard environment variables, when both halves are set.
    pub fn from_env() -> Option<Credentials> {
        let read = |name: &str| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
        Some(Credentials {
            access_key_id: read("AWS_ACCESS_KEY_ID")?,
            secret_access_key: read("AWS_SECRET_ACCESS_KEY")?,
            session_token: read("AWS_SESSION_TOKEN"),
        })
    }
}

/// Signs requests for one region and service.
#[derive(Clone, Debug)]
pub struct Signer {
    pub region: String,
    pub service: String,
    pub credentials: Credentials,
}

/// The headers a client's own signature put on a request.
const CLIENT_SIGNING_HEADERS: [&str; 4] = [
    "authorization",
    "x-amz-date",
    "x-amz-security-token",
    "x-amz-content-sha256",
];

/// Whether the client authenticates with a bearer token (a Bedrock API key),
/// which is forwarded rather than replaced by a signature.
pub fn uses_bearer(headers: &HeaderMap) -> bool {
    headers
        .get(http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.len() > 7 && v[..7].eq_ignore_ascii_case("bearer "))
}

impl Signer {
    /// Replace the client's signing headers on `headers` with a signature of
    /// `method url body` made now.
    pub fn sign(&self, method: &str, url: &str, headers: &mut HeaderMap, body: &[u8]) {
        self.sign_at(
            method,
            url,
            headers,
            body,
            &amz_date(std::time::SystemTime::now()),
        );
    }

    fn sign_at(
        &self,
        method: &str,
        url: &str,
        headers: &mut HeaderMap,
        body: &[u8],
        amz_date: &str,
    ) {
        for name in CLIENT_SIGNING_HEADERS {
            headers.remove(name);
        }
        let Ok(uri) = url.parse::<http::Uri>() else {
            return;
        };
        let host = match (uri.host(), uri.port_u16(), uri.scheme_str()) {
            (Some(host), Some(port), Some("https")) if port != 443 => format!("{host}:{port}"),
            (Some(host), Some(port), Some("http")) if port != 80 => format!("{host}:{port}"),
            (Some(host), _, _) => host.to_string(),
            (None, _, _) => return,
        };
        let payload = hex(digest::digest(&digest::SHA256, body).as_ref());
        let mut signed: Vec<(String, String)> = vec![
            ("host".into(), host),
            ("x-amz-content-sha256".into(), payload.clone()),
            ("x-amz-date".into(), amz_date.to_string()),
        ];
        if let Some(token) = &self.credentials.session_token {
            signed.push(("x-amz-security-token".into(), token.clone()));
        }
        if let Some(kind) = headers
            .get(http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
        {
            signed.push(("content-type".into(), kind.to_string()));
        }
        signed.sort();
        let canonical = canonical_request(
            method,
            uri.path(),
            uri.query().unwrap_or(""),
            &signed,
            &payload,
        );
        let date = &amz_date[..8];
        let scope = format!("{date}/{}/{}/aws4_request", self.region, self.service);
        let signature = signature(
            &self.credentials.secret_access_key,
            amz_date,
            &self.region,
            &self.service,
            &canonical,
        );
        let names: Vec<&str> = signed.iter().map(|(name, _)| name.as_str()).collect();
        let authorization = format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={}, Signature={signature}",
            self.credentials.access_key_id,
            names.join(";")
        );
        for (name, value) in signed
            .iter()
            .filter(|(name, _)| name != "host" && name != "content-type")
        {
            if let (Ok(name), Ok(value)) = (
                HeaderName::from_bytes(name.as_bytes()),
                HeaderValue::from_str(value),
            ) {
                headers.insert(name, value);
            }
        }
        if let Ok(value) = HeaderValue::from_str(&authorization) {
            headers.insert(http::header::AUTHORIZATION, value);
        }
    }
}

/// `YYYYMMDDTHHMMSSZ` for an instant.
pub fn amz_date(at: std::time::SystemTime) -> String {
    let secs = at
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let (days, rem) = (secs / 86_400, secs % 86_400);
    let (y, m, d) = civil_from_days(days as i64);
    format!(
        "{y:04}{m:02}{d:02}T{:02}{:02}{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// Days since 1970-01-01 to a proleptic Gregorian date (Howard Hinnant's
/// algorithm).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// RFC 3986 unreserved characters pass; everything else is `%XX`. `/` passes
/// in a path.
fn uri_encode(text: &str, keep_slash: bool) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            b'/' if keep_slash => out.push('/'),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Some(byte) = text
                .get(i + 1..i + 3)
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
        {
            out.push(byte);
            i += 3;
            continue;
        }
        out.push(if bytes[i] == b'+' { b' ' } else { bytes[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The canonical request. `path` is the path as it goes on the wire; every
/// service but S3 encodes it once more here, which is what AWS does with the
/// path it receives. `headers` are lower-cased and sorted.
pub fn canonical_request(
    method: &str,
    path: &str,
    query: &str,
    headers: &[(String, String)],
    payload_hash: &str,
) -> String {
    let path = if path.is_empty() { "/" } else { path };
    let mut pairs: Vec<(String, String)> = query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            (
                uri_encode(&percent_decode(key), false),
                uri_encode(&percent_decode(value), false),
            )
        })
        .collect();
    pairs.sort();
    let query = pairs
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&");
    let canonical_headers: String = headers
        .iter()
        .map(|(name, value)| {
            format!(
                "{name}:{}\n",
                value.split_whitespace().collect::<Vec<_>>().join(" ")
            )
        })
        .collect();
    let signed: Vec<&str> = headers.iter().map(|(name, _)| name.as_str()).collect();
    format!(
        "{method}\n{}\n{query}\n{canonical_headers}\n{}\n{payload_hash}",
        uri_encode(path, true),
        signed.join(";")
    )
}

fn hmac_sign(key: &[u8], data: &str) -> Vec<u8> {
    hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, key), data.as_bytes())
        .as_ref()
        .to_vec()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The signing key for a day, region and service.
pub fn signing_key(secret: &str, date: &str, region: &str, service: &str) -> Vec<u8> {
    let k_date = hmac_sign(format!("AWS4{secret}").as_bytes(), date);
    let k_region = hmac_sign(&k_date, region);
    let k_service = hmac_sign(&k_region, service);
    hmac_sign(&k_service, "aws4_request")
}

/// The hex signature of a canonical request.
pub fn signature(
    secret: &str,
    amz_date: &str,
    region: &str,
    service: &str,
    canonical: &str,
) -> String {
    let date = &amz_date[..8];
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{date}/{region}/{service}/aws4_request\n{}",
        hex(digest::digest(&digest::SHA256, canonical.as_bytes()).as_ref())
    );
    hex(&hmac_sign(
        &signing_key(secret, date, region, service),
        &string_to_sign,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY";

    /// AWS's worked example (IAM `ListUsers`, 2015-08-30), from the Signature
    /// Version 4 documentation: the signing key and the final signature.
    #[test]
    fn the_published_example_is_reproduced() {
        assert_eq!(
            hex(&signing_key(SECRET, "20150830", "us-east-1", "iam")),
            "c4afb1cc5771d871763a393e44b703571b55cc28424d1a5e86da6ed3c154a4b9"
        );
        let headers = vec![
            (
                "content-type".to_string(),
                "application/x-www-form-urlencoded; charset=utf-8".to_string(),
            ),
            ("host".to_string(), "iam.amazonaws.com".to_string()),
            ("x-amz-date".to_string(), "20150830T123600Z".to_string()),
        ];
        let canonical = canonical_request(
            "GET",
            "/",
            "Action=ListUsers&Version=2010-05-08",
            &headers,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        );
        assert_eq!(
            hex(digest::digest(&digest::SHA256, canonical.as_bytes()).as_ref()),
            "f536975d06c0309214f805bb90ccff089219ecd68b2577efef23edd43b7e1a59"
        );
        assert_eq!(
            signature(SECRET, "20150830T123600Z", "us-east-1", "iam", &canonical),
            "5d672d79c15b13162d9279b0855cfba6789a8edb4c82c400e06b5924a6f2b5d7"
        );
    }

    fn signer(token: Option<&str>) -> Signer {
        Signer {
            region: "us-east-1".into(),
            service: "bedrock".into(),
            credentials: Credentials {
                access_key_id: "AKIDEXAMPLE".into(),
                secret_access_key: SECRET.into(),
                session_token: token.map(str::to_string),
            },
        }
    }

    #[test]
    fn a_request_is_signed_over_the_body_that_goes_out_and_the_clients_signature_goes() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "authorization",
            HeaderValue::from_static("AWS4-HMAC-SHA256 stale"),
        );
        headers.insert("x-amz-date", HeaderValue::from_static("20000101T000000Z"));
        headers.insert("content-type", HeaderValue::from_static("application/json"));
        let url =
            "https://bedrock-runtime.us-east-1.amazonaws.com/model/anthropic.claude-v1:0/invoke";
        signer(Some("TOKEN")).sign_at("POST", url, &mut headers, b"{}", "20261126T120000Z");
        let auth = headers["authorization"].to_str().unwrap();
        assert!(
            auth.starts_with(
                "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20261126/us-east-1/bedrock/aws4_request, \
             SignedHeaders=content-type;host;x-amz-content-sha256;x-amz-date;x-amz-security-token, \
             Signature="
            ),
            "{auth}"
        );
        assert_eq!(headers["x-amz-date"], "20261126T120000Z");
        assert_eq!(headers["x-amz-security-token"], "TOKEN");
        assert_eq!(
            headers["x-amz-content-sha256"],
            "44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a",
            "sha256 of the body sent"
        );

        let mut other = HeaderMap::new();
        other.insert("content-type", HeaderValue::from_static("application/json"));
        signer(Some("TOKEN")).sign_at("POST", url, &mut other, b"{\"x\":1}", "20261126T120000Z");
        assert_ne!(
            other["authorization"], headers["authorization"],
            "the body is covered"
        );
    }

    #[test]
    fn the_path_is_encoded_once_more_for_the_canonical_request() {
        let canonical = canonical_request("POST", "/model/a.b-v1%3A0/invoke", "", &[], "x");
        assert!(
            canonical.starts_with("POST\n/model/a.b-v1%253A0/invoke\n\n"),
            "{canonical}"
        );
        let raw = canonical_request("POST", "/model/a.b-v1:0/invoke", "", &[], "x");
        assert!(raw.starts_with("POST\n/model/a.b-v1%3A0/invoke\n"), "{raw}");
    }

    #[test]
    fn a_bearer_key_is_recognised() {
        let mut headers = HeaderMap::new();
        assert!(!uses_bearer(&headers));
        headers.insert("authorization", HeaderValue::from_static("Bearer ABSK123"));
        assert!(uses_bearer(&headers));
        headers.insert(
            "authorization",
            HeaderValue::from_static("AWS4-HMAC-SHA256 x"),
        );
        assert!(!uses_bearer(&headers));
    }

    #[test]
    fn dates_format_as_amz_dates() {
        let at = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_440_938_160);
        assert_eq!(amz_date(at), "20150830T123600Z");
        assert_eq!(amz_date(std::time::UNIX_EPOCH), "19700101T000000Z");
        let leap = std::time::UNIX_EPOCH + std::time::Duration::from_secs(951_782_400);
        assert_eq!(amz_date(leap), "20000229T000000Z");
    }

    #[test]
    fn credentials_never_print_their_secret() {
        let text = format!("{:?}", signer(Some("TOKEN")).credentials);
        assert!(!text.contains(SECRET) && !text.contains("TOKEN"), "{text}");
    }
}
