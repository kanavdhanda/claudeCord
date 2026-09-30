//! A client for an S3-compatible bucket, used to keep old history files (and later backups) off the server's own disk.
//! Oracle Cloud Object Storage, Cloudflare R2, Backblaze B2 and AWS S3 all speak this protocol, so moving from one to
//! another is a change of settings, not of code. Requests are signed with AWS Signature Version 4.
//!
//! Only three operations exist: put, get and delete an object by key. That is all history needs.

use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::time::Duration;

/// Where a bucket is and how to sign in to it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Bucket {
    /// The service address without the bucket, for example `https://NAMESPACE.compat.objectstorage.REGION.oraclecloud.com`.
    pub endpoint: String,
    /// The signing region. Oracle uses its region name (`us-ashburn-1`), Cloudflare R2 uses `auto`.
    pub region: String,
    pub bucket: String,
    pub key_id: String,
    pub secret: String,
    /// Added in front of every key, to keep several things in one bucket tidy.
    #[serde(default)]
    pub prefix: String,
}

impl Bucket {
    /// The address settings for Oracle Cloud Object Storage's S3-compatible door.
    pub fn oracle(namespace: &str, region: &str, bucket: &str, key_id: &str, secret: &str) -> Self {
        Self {
            endpoint: format!("https://{namespace}.compat.objectstorage.{region}.oraclecloud.com"),
            region: region.into(),
            bucket: bucket.into(),
            key_id: key_id.into(),
            secret: secret.into(),
            prefix: String::new(),
        }
    }

    /// The address settings for Cloudflare R2.
    pub fn r2(account_id: &str, bucket: &str, key_id: &str, secret: &str) -> Self {
        Self {
            endpoint: format!("https://{account_id}.r2.cloudflarestorage.com"),
            region: "auto".into(),
            bucket: bucket.into(),
            key_id: key_id.into(),
            secret: secret.into(),
            prefix: String::new(),
        }
    }

    /// Uploads an object, replacing any with the same key.
    pub fn put(&self, key: &str, body: &[u8]) -> std::io::Result<()> {
        self.request("PUT", key, body).map(|_| ())
    }

    /// Downloads an object.
    pub fn get(&self, key: &str) -> std::io::Result<Vec<u8>> {
        self.request("GET", key, &[])
    }

    /// Deletes an object. Deleting one that is not there is not an error.
    pub fn delete(&self, key: &str) -> std::io::Result<()> {
        self.request("DELETE", key, &[]).map(|_| ())
    }

    /// Sends one signed request and returns the response body. Any status outside 2xx is an error carrying the status.
    fn request(&self, method: &str, key: &str, body: &[u8]) -> std::io::Result<Vec<u8>> {
        let path = format!("/{}/{}{}", self.bucket, self.prefix, key);
        let host = host_of(&self.endpoint);
        let payload_hash = hex(&Sha256::digest(body));
        let now = now_stamp();
        let auth = authorization(
            method,
            &host,
            &encode_path(&path),
            &[],
            &payload_hash,
            &self.key_id,
            &self.secret,
            &self.region,
            &now,
        );
        let url = format!(
            "{}{}",
            self.endpoint.trim_end_matches('/'),
            encode_path(&path)
        );
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .map_err(std::io::Error::other)?;
        let resp = client
            .request(method.parse().expect("valid method"), url)
            .header("x-amz-date", &now)
            .header("x-amz-content-sha256", &payload_hash)
            .header("authorization", auth)
            .body(body.to_vec())
            .send()
            .map_err(std::io::Error::other)?;
        let status = resp.status();
        let bytes = resp.bytes().map_err(std::io::Error::other)?.to_vec();
        if status.is_success() || (method == "DELETE" && status.as_u16() == 404) {
            Ok(bytes)
        } else {
            Err(std::io::Error::other(format!(
                "bucket answered {status}: {}",
                String::from_utf8_lossy(&bytes)
                    .chars()
                    .take(200)
                    .collect::<String>()
            )))
        }
    }
}

/// The `host[:port]` part of an address.
fn host_of(endpoint: &str) -> String {
    endpoint
        .split("://")
        .last()
        .unwrap_or(endpoint)
        .trim_end_matches('/')
        .split('/')
        .next()
        .unwrap_or("")
        .to_string()
}

/// The time as `YYYYMMDDTHHMMSSZ`, which is what the signature needs.
fn now_stamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    stamp(secs)
}

/// Formats seconds since the epoch as `YYYYMMDDTHHMMSSZ` (UTC), without a calendar library.
pub fn stamp(secs: i64) -> String {
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Civil-from-days (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}{m:02}{d:02}T{:02}{:02}{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// Percent-encodes a path the way the signature requires: everything except letters, digits and `-._~`, with `/` kept.
pub fn encode_path(path: &str) -> String {
    path.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b'/') {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn hmac(key: &[u8], data: &str) -> Vec<u8> {
    let mut m = <Hmac<Sha256> as KeyInit>::new_from_slice(key).expect("hmac takes any key length");
    m.update(data.as_bytes());
    m.finalize().into_bytes().to_vec()
}

/// The `Authorization` header value for a request, by AWS Signature Version 4. `extra` are headers besides the three
/// always signed (`host`, `x-amz-content-sha256`, `x-amz-date`). The path must already be encoded.
#[allow(clippy::too_many_arguments)]
pub fn authorization(
    method: &str,
    host: &str,
    path: &str,
    extra: &[(&str, &str)],
    payload_hash: &str,
    key_id: &str,
    secret: &str,
    region: &str,
    amz_date: &str,
) -> String {
    let mut headers: Vec<(String, String)> = vec![
        ("host".into(), host.into()),
        ("x-amz-content-sha256".into(), payload_hash.into()),
        ("x-amz-date".into(), amz_date.into()),
    ];
    headers.extend(
        extra
            .iter()
            .map(|(k, v)| (k.to_lowercase(), v.trim().to_string())),
    );
    headers.sort();
    let canonical_headers: String = headers.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
    let signed = headers
        .iter()
        .map(|(k, _)| k.as_str())
        .collect::<Vec<_>>()
        .join(";");
    let canonical = format!("{method}\n{path}\n\n{canonical_headers}\n{signed}\n{payload_hash}");
    let date = &amz_date[..8];
    let scope = format!("{date}/{region}/s3/aws4_request");
    let to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        hex(&Sha256::digest(canonical.as_bytes()))
    );
    let k = hmac(
        &hmac(
            &hmac(&hmac(format!("AWS4{secret}").as_bytes(), date), region),
            "s3",
        ),
        "aws4_request",
    );
    let signature = hex(&hmac(&k, &to_sign));
    format!(
        "AWS4-HMAC-SHA256 Credential={key_id}/{scope}, SignedHeaders={signed}, Signature={signature}"
    )
}
