use super::Fail;
use crate::filename;
use base64::Engine;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, ACCEPT_ENCODING, RANGE};
use reqwest::{Response, StatusCode};

pub const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/141.0.0.0 Safari/537.36";

#[derive(Clone, Debug, PartialEq)]
pub enum Digest {
    Sha256(Vec<u8>),
    Md5(Vec<u8>),
}

#[derive(Clone, Debug, Default)]
pub struct Head {
    pub url: String,
    pub size: Option<u64>,
    pub ranges: bool,
    pub name: Option<String>,
    pub mime: Option<String>,
    pub etag: Option<String>,
    pub modified: Option<String>,
    pub digest: Option<Digest>,
    pub attachment: bool,
}

impl Head {
    pub fn is_page(&self) -> bool {
        !self.attachment
            && self.mime.as_deref().is_some_and(|m| m.starts_with("text/html") || m.starts_with("application/xhtml"))
    }

    pub fn file_name(&self, fallback_url: &str) -> String {
        let mut name = self
            .name
            .clone()
            .or_else(|| filename::from_url(&self.url))
            .or_else(|| filename::from_url(fallback_url))
            .unwrap_or_else(|| "download".into());
        if !filename::has_ext(&name) {
            if let Some(ext) = self.mime.as_deref().and_then(filename::ext_for_mime) {
                name = format!("{name}.{ext}");
            }
        }
        filename::sanitize(&name)
    }
}

const SKIP: [&str; 8] = ["range", "accept-encoding", "host", "content-length", "connection", "if-range", "te", "upgrade"];

pub fn header_map(headers: &[(String, String)]) -> HeaderMap {
    let mut map = HeaderMap::new();
    for (k, v) in headers {
        if SKIP.contains(&k.to_ascii_lowercase().as_str()) || k.starts_with(':') {
            continue;
        }
        if let (Ok(k), Ok(v)) = (HeaderName::from_bytes(k.as_bytes()), HeaderValue::from_str(v)) {
            map.append(k, v);
        }
    }
    map
}

pub async fn open(client: &reqwest::Client, url: &str, headers: &HeaderMap) -> Result<(Head, Response), Fail> {
    let resp = client
        .get(url)
        .headers(headers.clone())
        .header(RANGE, "bytes=0-")
        .header(ACCEPT_ENCODING, "identity")
        .send()
        .await
        .map_err(Fail::net)?;
    let status = resp.status();
    if !(status.is_success()) {
        return Err(Fail::status(status));
    }
    Ok((read_head(&resp), resp))
}

pub async fn head(client: &reqwest::Client, url: &str, headers: &HeaderMap) -> Result<Head, Fail> {
    let resp = client
        .get(url)
        .headers(headers.clone())
        .header(RANGE, "bytes=0-0")
        .header(ACCEPT_ENCODING, "identity")
        .send()
        .await
        .map_err(Fail::net)?;
    let status = resp.status();
    if !status.is_success() {
        return Err(Fail::status(status));
    }
    Ok(read_head(&resp))
}

fn read_head(resp: &Response) -> Head {
    let h = resp.headers();
    let get = |n: &str| h.get(n).and_then(|v| v.to_str().ok()).map(str::to_string);
    let partial = resp.status() == StatusCode::PARTIAL_CONTENT;
    let total = get("content-range").and_then(|r| r.rsplit('/').next().and_then(|t| t.trim().parse::<u64>().ok()));
    let length = get("content-length").and_then(|l| l.parse::<u64>().ok());
    let size = if partial { total } else { length };
    let disposition = get("content-disposition");
    Head {
        url: resp.url().to_string(),
        size,
        ranges: partial && total.is_some(),
        name: disposition.as_deref().and_then(filename::from_disposition),
        mime: get("content-type"),
        etag: get("etag").filter(|e| !e.starts_with("W/")),
        modified: get("last-modified"),
        digest: digest(h, !partial),
        attachment: disposition.is_some_and(|d| d.trim().to_ascii_lowercase().starts_with("attachment")),
    }
}

fn digest(h: &HeaderMap, whole: bool) -> Option<Digest> {
    let b64 = |s: &str| base64::engine::general_purpose::STANDARD.decode(s.trim().trim_matches(':')).ok();
    let mut found = None;
    for name in ["repr-digest", "digest", "x-goog-hash"] {
        for v in h.get_all(name).iter().filter_map(|v| v.to_str().ok()) {
            for part in v.split(',') {
                let Some((alg, val)) = part.split_once('=') else { continue };
                match alg.trim().to_ascii_lowercase().as_str() {
                    "sha-256" => return b64(val).filter(|d| d.len() == 32).map(Digest::Sha256),
                    "md5" => found = found.or(b64(val).filter(|d| d.len() == 16).map(Digest::Md5)),
                    _ => {}
                }
            }
        }
    }
    if found.is_none() && whole {
        found = h
            .get("content-md5")
            .and_then(|v| v.to_str().ok())
            .and_then(b64)
            .filter(|d| d.len() == 16)
            .map(Digest::Md5);
    }
    found
}

pub fn verify(path: &std::path::Path, want: &Digest) -> std::io::Result<bool> {
    use md5::Digest as _;
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut buf = vec![0u8; 1 << 20];
    let mut sha = sha2::Sha256::new();
    let mut md = md5::Md5::new();
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        match want {
            Digest::Sha256(_) => sha2::Digest::update(&mut sha, &buf[..n]),
            Digest::Md5(_) => md.update(&buf[..n]),
        }
    }
    Ok(match want {
        Digest::Sha256(d) => sha2::Digest::finalize(sha).as_slice() == d.as_slice(),
        Digest::Md5(d) => md.finalize().as_slice() == d.as_slice(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_digests() {
        let mut h = HeaderMap::new();
        h.insert("x-goog-hash", "crc32c=n03x6A==, md5=Ojk9c3dhfxgoKVVHYwFbHQ==".parse().unwrap());
        assert!(matches!(digest(&h, false), Some(Digest::Md5(d)) if d.len() == 16));
        let mut h = HeaderMap::new();
        h.insert("repr-digest", "sha-256=:47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=:".parse().unwrap());
        assert!(matches!(digest(&h, false), Some(Digest::Sha256(_))));
        let mut h = HeaderMap::new();
        h.insert("content-md5", "Ojk9c3dhfxgoKVVHYwFbHQ==".parse().unwrap());
        assert!(digest(&h, false).is_none());
        assert!(digest(&h, true).is_some());
    }

    #[test]
    fn verifies_files() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x");
        std::fs::write(&p, b"").unwrap();
        let empty_sha = base64::engine::general_purpose::STANDARD.decode("47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=").unwrap();
        assert!(verify(&p, &Digest::Sha256(empty_sha)).unwrap());
        assert!(!verify(&p, &Digest::Md5(vec![0; 16])).unwrap());
    }

    #[test]
    fn skips_unsafe_headers() {
        let m = header_map(&[("Range".into(), "bytes=5-".into()), ("Cookie".into(), "a=b".into()), (":authority".into(), "x".into())]);
        assert_eq!(m.len(), 1);
        assert!(m.contains_key("cookie"));
    }
}
