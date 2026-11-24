//! An off-site copy of the backup archives, on S3-compatible object storage.
//!
//! A backup on the disk it backs up survives a bad migration, not a dead
//! disk. With `[maintenance.s3]` on, every scheduled archive is also written
//! to a bucket — AWS S3, MinIO, Cloudflare R2, Backblaze B2, Wasabi, Ceph —
//! together with its `.sha256`, so a copy fetched with any S3 client is
//! checked with `sha256sum -c` and restored with `kompressor restore`.
//!
//! # What this speaks
//!
//! Five calls of the S3 REST API, signed with Signature Version 4: `PutObject`
//! (an archive up to [`SINGLE_PUT_MAX`] goes in one, streamed from the file),
//! the multipart calls for a larger one, `ListObjectsV2` and `DeleteObject`.
//! There is no SDK behind it: the request is built and signed here, over the
//! HTTP client and the SHA-256 the crate already has, so a self-hosted store's
//! quirks are one place to read.
//!
//! Every request carries the real SHA-256 of its payload, not
//! `UNSIGNED-PAYLOAD`: the store checks the archive it received is the one
//! that was read, and a plain-HTTP MinIO accepts it as readily as AWS.
//!
//! # Safety
//!
//! Only names [`backup::archive_name`] writes (and their `.sha256`) are ever
//! deleted, and only under the configured prefix, so pointing this at a
//! bucket that holds other things cannot cost any of them. A failed upload
//! costs nothing already stored: the remote copies past `keep` are removed
//! only after the new one is in place.
//!
//! Everything here blocks, like the rest of [`backup`]: call it from
//! `spawn_blocking`.

use std::fs::File;
use std::path::Path;
use std::time::Duration;

use reqwest::Method;
use reqwest::blocking::{Body, Client};
use ring::{digest, hmac};
use serde::{Deserialize, Serialize};

use crate::backup;
use crate::config::{Config, S3Config};
use crate::error::CoreError;

/// The largest archive sent as one `PutObject`. Above this it goes in parts:
/// S3 takes 5 GiB in one, and a part is held in memory.
pub const SINGLE_PUT_MAX: u64 = 1 << 30;

/// The size of one part of a multipart upload. At 128 MiB the 10,000 parts S3
/// allows reach well past a terabyte.
pub const PART_SIZE: u64 = 128 << 20;

/// How long a call that moves no archive may take.
const CALL_TIMEOUT: Duration = Duration::from_secs(60);

/// How long one that does may take: an archive over a slow link.
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(3_600);

/// An archive in the bucket.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Remote {
    pub name: String,
    pub bytes: u64,
    /// When it was taken, from its name, as [`backup::Archive`] has it.
    pub created_at_ms: i64,
}

/// What an upload did.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Uploaded {
    pub name: String,
    pub bytes: u64,
    /// `s3://bucket/prefix/name`.
    pub destination: String,
    /// Older archives removed from the bucket to keep `keep`.
    pub removed: Vec<String>,
}

/// A client for the bucket `config` names.
pub struct S3 {
    config: S3Config,
    http: Client,
    single_max: u64,
    part_size: u64,
}

impl S3 {
    pub fn new(config: &S3Config) -> Result<S3, CoreError> {
        if let Some(problem) = config.problem() {
            return Err(CoreError::BadRequest(problem));
        }
        let http = Client::builder()
            .user_agent(concat!("kompressor/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(15))
            .build()
            .map_err(|error| CoreError::ObjectStore(format!("http client: {error}")))?;
        Ok(S3 {
            config: config.clone(),
            http,
            single_max: SINGLE_PUT_MAX,
            part_size: PART_SIZE,
        })
    }

    /// Where an object called `name` is, for people: `s3://bucket/prefix/name`.
    pub fn destination(&self, name: &str) -> String {
        format!("s3://{}/{}{}", self.config.bucket, self.prefix(), name)
    }

    /// The key prefix, `""` or ending in a slash.
    fn prefix(&self) -> String {
        let trimmed = self.config.prefix.trim().trim_matches('/');
        if trimmed.is_empty() {
            String::new()
        } else {
            format!("{trimmed}/")
        }
    }

    fn key(&self, name: &str) -> String {
        format!("{}{}", self.prefix(), name)
    }

    /// Check the bucket can be listed and written to: list it, then write
    /// and remove a small object. Returns how long it took.
    pub fn probe(&self) -> Result<Duration, CoreError> {
        let started = std::time::Instant::now();
        self.list_page(None, 1)?;
        let name = ".kompressor-probe";
        self.put_bytes(name, b"kompressor can write here\n")?;
        // A key that may write and not delete is a configuration `keep`
        // would trip over, but not one that stops backups.
        if let Err(error) = self.delete(name) {
            tracing::warn!(%error, "the probe object was written but could not be removed");
        }
        Ok(started.elapsed())
    }

    /// The archives in the bucket under the prefix, newest first.
    pub fn list(&self) -> Result<Vec<Remote>, CoreError> {
        let mut found = Vec::new();
        let mut token = None;
        loop {
            let (mut page, next) = self.list_page(token.as_deref(), 1_000)?;
            found.append(&mut page);
            match next {
                Some(next) => token = Some(next),
                None => break,
            }
        }
        found.sort_by_key(|remote| std::cmp::Reverse(remote.created_at_ms));
        Ok(found)
    }

    fn list_page(
        &self,
        token: Option<&str>,
        max: u32,
    ) -> Result<(Vec<Remote>, Option<String>), CoreError> {
        let mut query = vec![
            ("list-type".to_string(), "2".to_string()),
            ("max-keys".to_string(), max.to_string()),
            ("prefix".to_string(), self.prefix()),
        ];
        if let Some(token) = token {
            query.push(("continuation-token".to_string(), token.to_string()));
        }
        let body = self
            .call(Method::GET, None, &query, empty_hash(), None, CALL_TIMEOUT)?
            .text()
            .map_err(|error| CoreError::ObjectStore(format!("listing: {error}")))?;
        let prefix = self.prefix();
        let remotes = blocks(&body, "Contents")
            .filter_map(|entry| {
                let key = unescape(tag(entry, "Key")?);
                let name = key.strip_prefix(&prefix)?;
                let created_at_ms = backup::archive_time(name)?;
                Some(Remote {
                    name: name.to_string(),
                    bytes: tag(entry, "Size")?.trim().parse().ok()?,
                    created_at_ms,
                })
            })
            .collect();
        let next = (tag(&body, "IsTruncated").map(str::trim) == Some("true"))
            .then(|| tag(&body, "NextContinuationToken").map(unescape))
            .flatten();
        Ok((remotes, next))
    }

    pub fn delete(&self, name: &str) -> Result<(), CoreError> {
        self.call(
            Method::DELETE,
            Some(&self.key(name)),
            &[],
            empty_hash(),
            None,
            CALL_TIMEOUT,
        )
        .map(drop)
    }

    pub fn put_bytes(&self, name: &str, bytes: &[u8]) -> Result<(), CoreError> {
        self.call(
            Method::PUT,
            Some(&self.key(name)),
            &[],
            &sha256_hex(bytes),
            Some(Body::from(bytes.to_vec())),
            CALL_TIMEOUT,
        )
        .map(drop)
    }

    /// Write the file at `path` as the object `name`; whole when it is small
    /// enough, in parts when it is not. `sha256` is the file's digest, which
    /// the single request signs.
    pub fn put_file(&self, path: &Path, name: &str, sha256: &str) -> Result<u64, CoreError> {
        let file = File::open(path)?;
        let bytes = file.metadata()?.len();
        if bytes <= self.single_max {
            self.call(
                Method::PUT,
                Some(&self.key(name)),
                &[],
                sha256,
                Some(Body::sized(file, bytes)),
                UPLOAD_TIMEOUT,
            )?;
        } else {
            self.put_parts(file, bytes, name)?;
        }
        Ok(bytes)
    }

    /// A multipart upload, aborted if any part fails so the store is not left
    /// holding (and billing) the pieces.
    fn put_parts(&self, mut file: File, bytes: u64, name: &str) -> Result<(), CoreError> {
        let key = self.key(name);
        let started = self
            .call(
                Method::POST,
                Some(&key),
                &[("uploads".to_string(), String::new())],
                empty_hash(),
                None,
                CALL_TIMEOUT,
            )?
            .text()
            .map_err(|error| CoreError::ObjectStore(format!("multipart start: {error}")))?;
        let id = tag(&started, "UploadId")
            .map(unescape)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| {
                CoreError::ObjectStore("the store did not return an upload id".to_string())
            })?;
        let uploaded = self.send_parts(&mut file, bytes, &key, &id);
        if uploaded.is_err() {
            let _ = self.call(
                Method::DELETE,
                Some(&key),
                &[("uploadId".to_string(), id.clone())],
                empty_hash(),
                None,
                CALL_TIMEOUT,
            );
        }
        uploaded
    }

    fn send_parts(
        &self,
        file: &mut File,
        bytes: u64,
        key: &str,
        id: &str,
    ) -> Result<(), CoreError> {
        use std::io::Read;
        let mut etags = Vec::new();
        let mut left = bytes;
        while left > 0 {
            let size = left.min(self.part_size);
            let mut part = vec![0u8; size as usize];
            file.read_exact(&mut part)?;
            let number = etags.len() + 1;
            let response = self.call(
                Method::PUT,
                Some(key),
                &[
                    ("partNumber".to_string(), number.to_string()),
                    ("uploadId".to_string(), id.to_string()),
                ],
                &sha256_hex(&part),
                Some(Body::from(part)),
                UPLOAD_TIMEOUT,
            )?;
            let etag = response
                .headers()
                .get("etag")
                .and_then(|value| value.to_str().ok())
                .map(str::to_string)
                .ok_or_else(|| {
                    CoreError::ObjectStore(format!("part {number} came back without an ETag"))
                })?;
            etags.push(etag);
            left -= size;
        }
        let mut manifest = String::from("<CompleteMultipartUpload>");
        for (index, etag) in etags.iter().enumerate() {
            manifest.push_str(&format!(
                "<Part><PartNumber>{}</PartNumber><ETag>{}</ETag></Part>",
                index + 1,
                escape(etag)
            ));
        }
        manifest.push_str("</CompleteMultipartUpload>");
        let done = self
            .call(
                Method::POST,
                Some(key),
                &[("uploadId".to_string(), id.to_string())],
                &sha256_hex(manifest.as_bytes()),
                Some(Body::from(manifest)),
                CALL_TIMEOUT,
            )?
            .text()
            .unwrap_or_default();
        // Completing can answer 200 and carry an error in the body.
        if done.contains("<Error>") {
            return Err(CoreError::ObjectStore(format!(
                "completing the upload: {}",
                describe_error(&done)
            )));
        }
        Ok(())
    }

    /// Where a request goes: the URL, the host it is signed for, and the
    /// canonical path.
    fn target(&self, key: Option<&str>) -> Result<(String, String, String), CoreError> {
        let base = if self.config.endpoint.trim().is_empty() {
            format!("https://s3.{}.amazonaws.com", self.config.region.trim())
        } else {
            self.config.endpoint.trim().to_string()
        };
        let url = url::Url::parse(&base)
            .map_err(|error| CoreError::ObjectStore(format!("endpoint: {error}")))?;
        let host = url
            .host_str()
            .ok_or_else(|| CoreError::ObjectStore("the endpoint has no host".to_string()))?;
        let mut authority = host.to_string();
        if let Some(port) = url.port() {
            authority = format!("{authority}:{port}");
        }
        let base_path = url.path().trim_end_matches('/');
        let bucket = self.config.bucket.trim();
        let (authority, path) = if self.config.path_style {
            (
                authority,
                format!(
                    "{base_path}/{bucket}{}",
                    key.map_or(String::new(), |k| format!("/{k}"))
                ),
            )
        } else {
            (
                format!("{bucket}.{authority}"),
                format!("{base_path}/{}", key.unwrap_or("")),
            )
        };
        let path = encode(&path, true);
        Ok((format!("{}://{authority}", url.scheme()), authority, path))
    }

    /// Send one signed request and return the response when it succeeded.
    fn call(
        &self,
        method: Method,
        key: Option<&str>,
        query: &[(String, String)],
        payload_hash: &str,
        body: Option<Body>,
        timeout: Duration,
    ) -> Result<reqwest::blocking::Response, CoreError> {
        let (origin, host, path) = self.target(key)?;
        let mut pairs: Vec<(String, String)> = query
            .iter()
            .map(|(name, value)| (encode(name, false), encode(value, false)))
            .collect();
        pairs.sort();
        let canonical_query = pairs
            .iter()
            .map(|(name, value)| format!("{name}={value}"))
            .collect::<Vec<_>>()
            .join("&");
        let now = backup::civil(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|since| since.as_millis() as i64)
                .unwrap_or(0),
        );
        let amz_date = format!(
            "{:04}{:02}{:02}T{:02}{:02}{:02}Z",
            now.0, now.1, now.2, now.3, now.4, now.5
        );
        let headers = [
            ("host".to_string(), host),
            ("x-amz-content-sha256".to_string(), payload_hash.to_string()),
            ("x-amz-date".to_string(), amz_date.clone()),
        ];
        let signed = sign(
            method.as_str(),
            &path,
            &canonical_query,
            &headers,
            payload_hash,
            &Credentials {
                access_key_id: &self.config.access_key_id,
                secret_access_key: &self.config.secret_access_key,
                region: self.config.region.trim(),
            },
            &amz_date,
        );
        let url = if canonical_query.is_empty() {
            format!("{origin}{path}")
        } else {
            format!("{origin}{path}?{canonical_query}")
        };
        let mut request = self
            .http
            .request(method.clone(), &url)
            .timeout(timeout)
            .header("x-amz-content-sha256", payload_hash)
            .header("x-amz-date", &amz_date)
            .header("authorization", signed);
        request = match body {
            Some(body) => request.body(body),
            None => request,
        };
        let what = format!("{method} {}", key.unwrap_or("(bucket)"));
        let response = request
            .send()
            .map_err(|error| CoreError::ObjectStore(format!("{what}: {}", error.without_url())))?;
        if response.status().is_success() {
            return Ok(response);
        }
        let status = response.status();
        let text = response.text().unwrap_or_default();
        Err(CoreError::ObjectStore(format!(
            "{what}: {status} {}",
            describe_error(&text)
        )))
    }
}

/// The part of an S3 error body worth showing: its code and message.
fn describe_error(body: &str) -> String {
    let code = tag(body, "Code").map(unescape);
    let message = tag(body, "Message").map(unescape);
    match (code, message) {
        (Some(code), Some(message)) => format!("{code}: {message}"),
        (Some(code), None) => code,
        _ => {
            let trimmed: String = body.trim().chars().take(200).collect();
            trimmed
        }
    }
}

struct Credentials<'a> {
    access_key_id: &'a str,
    secret_access_key: &'a str,
    region: &'a str,
}

/// The `Authorization` header value for a Signature Version 4 request.
/// `headers` are the ones signed, lowercase-named; `canonical_path` and
/// `canonical_query` are already encoded.
fn sign(
    method: &str,
    canonical_path: &str,
    canonical_query: &str,
    headers: &[(String, String)],
    payload_hash: &str,
    credentials: &Credentials,
    amz_date: &str,
) -> String {
    let mut sorted: Vec<&(String, String)> = headers.iter().collect();
    sorted.sort();
    let canonical_headers: String = sorted
        .iter()
        .map(|(name, value)| format!("{name}:{}\n", value.trim()))
        .collect();
    let signed_headers = sorted
        .iter()
        .map(|(name, _)| name.as_str())
        .collect::<Vec<_>>()
        .join(";");
    let canonical = format!(
        "{method}\n{canonical_path}\n{canonical_query}\n{canonical_headers}\n{signed_headers}\n{payload_hash}"
    );
    let day = &amz_date[..8];
    let scope = format!("{day}/{}/s3/aws4_request", credentials.region);
    let to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        sha256_hex(canonical.as_bytes())
    );
    let mut key = hmac_sha256(
        format!("AWS4{}", credentials.secret_access_key).as_bytes(),
        day.as_bytes(),
    );
    for part in [credentials.region, "s3", "aws4_request"] {
        key = hmac_sha256(&key, part.as_bytes());
    }
    let signature = hex(&hmac_sha256(&key, to_sign.as_bytes()));
    format!(
        "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
        credentials.access_key_id
    )
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, key), data)
        .as_ref()
        .to_vec()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn sha256_hex(data: &[u8]) -> String {
    hex(digest::digest(&digest::SHA256, data).as_ref())
}

fn empty_hash() -> &'static str {
    "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
}

/// Percent-encode as SigV4 wants: everything but `A-Za-z0-9-_.~`, and the
/// slash too unless `keep_slash`.
fn encode(text: &str, keep_slash: bool) -> String {
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

/// The text of the first `<name>…</name>` in `xml`.
fn tag<'a>(xml: &'a str, name: &str) -> Option<&'a str> {
    let open = format!("<{name}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&format!("</{name}>"))?;
    Some(&xml[start..start + end])
}

/// The text between each `<name>` and `</name>` in `xml`.
fn blocks<'a>(xml: &'a str, name: &'a str) -> impl Iterator<Item = &'a str> {
    let mut rest = xml;
    std::iter::from_fn(move || {
        let found = tag(rest, name)?;
        let close = format!("</{name}>");
        let end = rest.find(&close)? + close.len();
        rest = &rest[end..];
        Some(found)
    })
}

fn unescape(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Copy `archive` to the bucket as `name`, with its checksum beside it, then
/// remove what is past `keep`. The archive is read, not changed.
pub fn upload_archive(config: &Config, archive: &Path, name: &str) -> Result<Uploaded, CoreError> {
    if backup::archive_time(name).is_none() {
        return Err(CoreError::BadRequest(format!(
            "{name:?} is not a name this writes archives under"
        )));
    }
    let s3 = S3::new(&config.maintenance.s3)?;
    let sha256 = backup::sha256_file(archive)?;
    let bytes = s3.put_file(archive, name, &sha256)?;
    // The checksum goes after the archive: one there, in the bucket, is
    // proof the archive finished uploading.
    s3.put_bytes(
        &format!("{name}.sha256"),
        format!("{sha256}  {name}\n").as_bytes(),
    )?;
    let mut removed = Vec::new();
    if config.maintenance.s3.keep > 0 {
        // Pruning is housekeeping: an archive that is up is not a failed
        // backup because an older one could not be removed.
        match s3.list() {
            Ok(remote) => {
                for old in remote.iter().skip(config.maintenance.s3.keep as usize) {
                    if s3.delete(&old.name).is_ok() {
                        let _ = s3.delete(&format!("{}.sha256", old.name));
                        removed.push(old.name.clone());
                    }
                }
            }
            Err(error) => tracing::warn!(%error, "the bucket's old archives were not pruned"),
        }
    }
    Ok(Uploaded {
        name: name.to_string(),
        bytes,
        destination: s3.destination(name),
        removed,
    })
}

/// Make sure the newest archive in `maintenance.backup_dir` is in the bucket,
/// uploading it if it is not. `Ok(None)` when there is nothing to upload;
/// otherwise its name, and what the upload did if one was needed.
///
/// What a scheduled backup calls after writing one — and what retries an
/// upload that failed, or catches up after the bucket was turned on, without
/// taking another backup.
pub fn sync_newest(config: &Config) -> Result<Option<(String, Option<Uploaded>)>, CoreError> {
    let dir = &config.maintenance.backup_dir;
    if dir.as_os_str().is_empty() {
        return Ok(None);
    }
    let Some(newest) = backup::list_archives(dir).into_iter().next() else {
        return Ok(None);
    };
    let s3 = S3::new(&config.maintenance.s3)?;
    if s3.list()?.iter().any(|remote| remote.name == newest.name) {
        return Ok(Some((newest.name, None)));
    }
    let uploaded = upload_archive(config, &dir.join(&newest.name), &newest.name)?;
    Ok(Some((newest.name, Some(uploaded))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_signature_matches_the_one_aws_publishes() {
        // "Example 1: GET Object" from the Signature Version 4 documentation.
        let headers = [
            (
                "host".to_string(),
                "examplebucket.s3.amazonaws.com".to_string(),
            ),
            ("range".to_string(), "bytes=0-9".to_string()),
            ("x-amz-content-sha256".to_string(), empty_hash().to_string()),
            ("x-amz-date".to_string(), "20130524T000000Z".to_string()),
        ];
        let header = sign(
            "GET",
            "/test.txt",
            "",
            &headers,
            empty_hash(),
            &Credentials {
                access_key_id: "AKIAIOSFODNN7EXAMPLE",
                secret_access_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
                region: "us-east-1",
            },
            "20130524T000000Z",
        );
        assert_eq!(
            header,
            concat!(
                "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request, ",
                "SignedHeaders=host;range;x-amz-content-sha256;x-amz-date, ",
                "Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
            )
        );
    }

    #[test]
    fn keys_are_encoded_the_way_the_signature_wants() {
        assert_eq!(encode("a b/c+d~e", true), "a%20b/c%2Bd~e");
        assert_eq!(encode("a/b", false), "a%2Fb");
    }

    #[test]
    fn a_listing_is_read_for_archives_and_nothing_else() {
        let xml = "<ListBucketResult><IsTruncated>false</IsTruncated>\
            <Contents><Key>kompressor/kompressor-backup-20261120-071530.tar.gz</Key><Size>42</Size></Contents>\
            <Contents><Key>kompressor/kompressor-backup-20261120-071530.tar.gz.sha256</Key><Size>90</Size></Contents>\
            <Contents><Key>kompressor/notes.txt</Key><Size>1</Size></Contents>\
            </ListBucketResult>";
        let found: Vec<_> = blocks(xml, "Contents")
            .filter_map(|entry| {
                let key = unescape(tag(entry, "Key")?);
                let name = key.strip_prefix("kompressor/")?;
                backup::archive_time(name)?;
                Some(name.to_string())
            })
            .collect();
        assert_eq!(found, vec!["kompressor-backup-20261120-071530.tar.gz"]);
    }

    #[test]
    fn the_prefix_is_kept_to_a_folder_whatever_it_was_written_as() {
        let client = |prefix: &str| {
            let mut config = S3Config {
                enabled: true,
                bucket: "b".into(),
                access_key_id: "k".into(),
                secret_access_key: "s".into(),
                ..S3Config::default()
            };
            config.prefix = prefix.into();
            S3::new(&config).expect("valid")
        };
        assert_eq!(client("").prefix(), "");
        assert_eq!(client("/").prefix(), "");
        assert_eq!(client("backups").prefix(), "backups/");
        assert_eq!(client("/a/b/").prefix(), "a/b/");
        assert_eq!(client("x").destination("n"), "s3://b/x/n");
    }

    #[test]
    fn settings_that_cannot_work_are_named() {
        let mut config = S3Config::default();
        assert!(config.problem().expect("no bucket").contains("bucket"));
        config.bucket = "b".into();
        assert!(config.problem().expect("no keys").contains("access_key_id"));
        config.access_key_id = "k".into();
        config.secret_access_key = "s".into();
        assert_eq!(config.problem(), None);
        config.endpoint = "ftp://x".into();
        assert!(config.problem().expect("scheme").contains("endpoint"));
        assert!(
            !format!("{:?}", config).contains("\"s\""),
            "secret in Debug"
        );
    }

    use std::collections::HashMap;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    /// What the fake store holds, and every request it saw.
    #[derive(Default)]
    struct Store {
        objects: HashMap<String, Vec<u8>>,
        uploads: HashMap<String, Vec<(u32, Vec<u8>)>>,
        seen: Vec<String>,
        /// Refuse every request with this status, when set.
        refuse: Option<u16>,
    }

    /// A store that speaks enough S3 to be uploaded to: it checks each
    /// request is signed and that its payload is the one signed for.
    fn fake_store() -> (String, Arc<Mutex<Store>>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("a port");
        let url = format!("http://{}", listener.local_addr().expect("addr"));
        let store = Arc::new(Mutex::new(Store::default()));
        let shared = store.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let store = shared.clone();
                std::thread::spawn(move || serve(stream, store));
            }
        });
        (url, store)
    }

    fn serve(stream: std::net::TcpStream, store: Arc<Mutex<Store>>) {
        let mut reader = BufReader::new(stream.try_clone().expect("clone"));
        let mut out = stream;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                return;
            }
            let mut parts = line.split_whitespace();
            let (method, target) = (
                parts.next().unwrap_or("").to_string(),
                parts.next().unwrap_or("").to_string(),
            );
            let mut headers = HashMap::new();
            loop {
                let mut header = String::new();
                reader.read_line(&mut header).expect("header");
                let header = header.trim_end().to_string();
                if header.is_empty() {
                    break;
                }
                if let Some((name, value)) = header.split_once(':') {
                    headers.insert(name.to_ascii_lowercase(), value.trim().to_string());
                }
            }
            let length: usize = headers
                .get("content-length")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            let mut body = vec![0u8; length];
            reader.read_exact(&mut body).expect("body");
            let (path, query) = target.split_once('?').unwrap_or((&target, ""));
            let query: HashMap<&str, &str> = query
                .split('&')
                .filter(|pair| !pair.is_empty())
                .map(|pair| pair.split_once('=').unwrap_or((pair, "")))
                .collect();
            let mut store = store.lock().expect("lock");
            store.seen.push(format!("{method} {target}"));
            let signed = headers
                .get("authorization")
                .is_some_and(|a| a.starts_with("AWS4-HMAC-SHA256 Credential=AK/"))
                && headers.get("x-amz-content-sha256") == Some(&sha256_hex(&body));
            let (status, reply, extra): (u16, String, String) = if !signed {
                (
                    403,
                    "<Error><Code>SignatureDoesNotMatch</Code><Message>bad</Message></Error>"
                        .into(),
                    String::new(),
                )
            } else if let Some(code) = store.refuse {
                (
                    code,
                    "<Error><Code>AccessDenied</Code><Message>no</Message></Error>".into(),
                    String::new(),
                )
            } else {
                let key = path.trim_start_matches("/bucket/").to_string();
                match (
                    method.as_str(),
                    query.contains_key("uploads"),
                    query.get("uploadId"),
                    query.get("partNumber"),
                ) {
                    ("POST", true, _, _) => {
                        store.uploads.insert("u1".into(), Vec::new());
                        (200, "<InitiateMultipartUploadResult><UploadId>u1</UploadId></InitiateMultipartUploadResult>".into(), String::new())
                    }
                    ("PUT", _, Some(_), Some(number)) => {
                        let number: u32 = number.parse().expect("number");
                        store
                            .uploads
                            .get_mut("u1")
                            .expect("started")
                            .push((number, body));
                        (200, String::new(), format!("ETag: \"etag{number}\"\r\n"))
                    }
                    ("POST", _, Some(_), _) => {
                        let mut parts = store.uploads.remove("u1").expect("started");
                        parts.sort();
                        let whole: Vec<u8> =
                            parts.into_iter().flat_map(|(_, bytes)| bytes).collect();
                        store.objects.insert(key, whole);
                        (
                            200,
                            "<CompleteMultipartUploadResult/>".into(),
                            String::new(),
                        )
                    }
                    ("DELETE", _, Some(_), _) => {
                        store.uploads.remove("u1");
                        (204, String::new(), String::new())
                    }
                    ("PUT", ..) => {
                        store.objects.insert(key, body);
                        (200, String::new(), String::new())
                    }
                    ("DELETE", ..) => {
                        store.objects.remove(&key);
                        (204, String::new(), String::new())
                    }
                    ("GET", ..) => {
                        let prefix = query
                            .get("prefix")
                            .copied()
                            .unwrap_or("")
                            .replace("%2F", "/");
                        let mut keys: Vec<_> = store.objects.iter().collect();
                        keys.sort();
                        let items: String = keys
                            .into_iter()
                            .filter(|(k, _)| k.starts_with(prefix.as_str()))
                            .map(|(k, v)| {
                                format!(
                                    "<Contents><Key>{k}</Key><Size>{}</Size></Contents>",
                                    v.len()
                                )
                            })
                            .collect();
                        (
                            200,
                            format!(
                                "<ListBucketResult><IsTruncated>false</IsTruncated>{items}</ListBucketResult>"
                            ),
                            String::new(),
                        )
                    }
                    _ => (400, String::new(), String::new()),
                }
            };
            let _ = write!(
                out,
                "HTTP/1.1 {status} X\r\n{extra}Content-Length: {}\r\n\r\n{reply}",
                reply.len()
            );
        }
    }

    fn configured(endpoint: &str, keep: u32) -> Config {
        let mut config = Config::default();
        config.maintenance.s3 = S3Config {
            enabled: true,
            endpoint: endpoint.to_string(),
            bucket: "bucket".into(),
            prefix: "kompressor".into(),
            access_key_id: "AK".into(),
            secret_access_key: "SK".into(),
            keep,
            ..S3Config::default()
        };
        config
    }

    #[test]
    fn an_archive_goes_up_with_its_checksum_and_the_old_ones_are_pruned() {
        let (url, store) = fake_store();
        let config = configured(&url, 2);
        let dir = tempfile::tempdir().expect("dir");
        for (index, ms) in [1_000_000_000_000_i64, 1_000_000_100_000, 1_000_000_200_000]
            .into_iter()
            .enumerate()
        {
            let name = backup::archive_name(ms);
            let path = dir.path().join(&name);
            std::fs::write(&path, format!("archive {index}")).expect("write");
            let done = upload_archive(&config, &path, &name).expect("uploaded");
            assert_eq!(done.bytes, format!("archive {index}").len() as u64);
            assert_eq!(done.destination, format!("s3://bucket/kompressor/{name}"));
        }
        let kept = S3::new(&config.maintenance.s3)
            .expect("client")
            .list()
            .expect("list");
        assert_eq!(kept.len(), 2, "keep = 2: {kept:?}");
        let held = store.lock().expect("lock");
        assert!(
            held.objects
                .keys()
                .all(|k| k.starts_with("kompressor/kompressor-backup-"))
        );
        assert_eq!(held.objects.len(), 4, "two archives and their checksums");
        let newest = backup::archive_name(1_000_000_200_000);
        let sum = held.objects[&format!("kompressor/{newest}.sha256")].clone();
        assert_eq!(
            String::from_utf8(sum).expect("text"),
            format!("{}  {newest}\n", sha256_hex(b"archive 2"))
        );
    }

    #[test]
    fn a_large_archive_is_sent_in_parts_and_comes_back_whole() {
        let (url, store) = fake_store();
        let config = configured(&url, 0);
        let mut client = S3::new(&config.maintenance.s3).expect("client");
        client.single_max = 10;
        client.part_size = 7;
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("big");
        let content: Vec<u8> = (0..100u8).collect();
        std::fs::write(&path, &content).expect("write");
        let name = backup::archive_name(1_000_000_000_000);
        let sent = client
            .put_file(&path, &name, &sha256_hex(&content))
            .expect("sent");
        assert_eq!(sent, 100);
        assert_eq!(
            store.lock().expect("lock").objects[&format!("kompressor/{name}")],
            content
        );
    }

    #[test]
    fn a_refusal_is_reported_with_the_stores_reason_and_no_key() {
        let (url, store) = fake_store();
        store.lock().expect("lock").refuse = Some(403);
        let config = configured(&url, 0);
        let error = S3::new(&config.maintenance.s3)
            .expect("client")
            .probe()
            .expect_err("refused")
            .to_string();
        assert!(
            error.contains("403") && error.contains("AccessDenied: no"),
            "{error}"
        );
        assert!(!error.contains("SK"), "{error}");
    }

    #[test]
    fn the_newest_local_archive_is_uploaded_once() {
        let (url, store) = fake_store();
        let dir = tempfile::tempdir().expect("dir");
        let mut config = configured(&url, 0);
        config.maintenance.backup_dir = dir.path().to_path_buf();
        assert!(sync_newest(&config).expect("nothing to do").is_none());
        let name = backup::archive_name(1_000_000_000_000);
        std::fs::write(dir.path().join(&name), b"one").expect("write");
        let (synced, uploaded) = sync_newest(&config).expect("synced").expect("one");
        assert_eq!(synced, name);
        assert!(uploaded.is_some());
        let (_, again) = sync_newest(&config).expect("synced").expect("one");
        assert!(again.is_none(), "already in the bucket");
        let puts = store
            .lock()
            .expect("lock")
            .seen
            .iter()
            .filter(|r| r.starts_with("PUT"))
            .count();
        assert_eq!(puts, 2, "the archive and its checksum, once");
    }
}
