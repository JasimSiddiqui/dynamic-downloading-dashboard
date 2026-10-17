use percent_encoding::percent_decode_str;
use std::path::{Path, PathBuf};

pub fn from_disposition(value: &str) -> Option<String> {
    let mut plain = None;
    for part in split_params(value) {
        let (key, val) = match part.split_once('=') {
            Some((k, v)) => (k.trim().to_ascii_lowercase(), v.trim()),
            None => continue,
        };
        if key == "filename*" {
            let val = val.trim_matches('"');
            let encoded = match val.splitn(3, '\'').collect::<Vec<_>>().as_slice() {
                [_, _, rest] => *rest,
                _ => val,
            };
            let decoded = percent_decode_str(encoded).decode_utf8_lossy().into_owned();
            if !decoded.is_empty() {
                return Some(decoded);
            }
        } else if key == "filename" {
            let v = unquote(val);
            if !v.is_empty() {
                plain = Some(percent_decode_str(&v).decode_utf8_lossy().into_owned());
            }
        }
    }
    plain
}

fn split_params(value: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut quoted = false;
    for (i, c) in value.char_indices() {
        match c {
            '"' => quoted = !quoted,
            ';' if !quoted => {
                out.push(&value[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&value[start..]);
    out
}

fn unquote(v: &str) -> String {
    let v = v.trim();
    if v.len() >= 2 && v.starts_with('"') && v.ends_with('"') {
        v[1..v.len() - 1].replace("\\\"", "\"").replace("\\\\", "\\")
    } else {
        v.to_string()
    }
}

pub fn from_url(url: &str) -> Option<String> {
    let u = url::Url::parse(url).ok()?;
    let last = u.path_segments()?.filter(|s| !s.is_empty()).last()?;
    let name = percent_decode_str(last).decode_utf8_lossy().into_owned();
    (!name.is_empty()).then_some(name)
}

pub fn ext_for_mime(mime: &str) -> Option<&'static str> {
    let m = mime.split(';').next()?.trim().to_ascii_lowercase();
    Some(match m.as_str() {
        "application/pdf" => "pdf",
        "application/zip" | "application/x-zip-compressed" => "zip",
        "application/x-7z-compressed" => "7z",
        "application/x-rar-compressed" | "application/vnd.rar" => "rar",
        "application/gzip" | "application/x-gzip" => "gz",
        "application/x-tar" => "tar",
        "application/epub+zip" => "epub",
        "application/x-msdownload" | "application/vnd.microsoft.portable-executable" => "exe",
        "application/x-msi" => "msi",
        "application/x-iso9660-image" => "iso",
        "application/json" => "json",
        "application/x-bittorrent" => "torrent",
        "video/mp4" => "mp4",
        "video/webm" => "webm",
        "video/x-matroska" => "mkv",
        "video/quicktime" => "mov",
        "video/mp2t" => "ts",
        "audio/mpeg" => "mp3",
        "audio/mp4" => "m4a",
        "audio/ogg" => "ogg",
        "audio/flac" => "flac",
        "audio/wav" | "audio/x-wav" => "wav",
        "image/jpeg" => "jpg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/avif" => "avif",
        "text/plain" => "txt",
        "text/csv" => "csv",
        _ => return None,
    })
}

const RESERVED: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9", "LPT1",
    "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

pub fn sanitize(name: &str) -> String {
    let mut s: String = name
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if (c as u32) < 32 => '_',
            c => c,
        })
        .collect();
    s = s.trim().trim_end_matches(['.', ' ']).to_string();
    if s.is_empty() {
        s = "download".into();
    }
    let stem = s.split('.').next().unwrap_or("").to_ascii_uppercase();
    if RESERVED.contains(&stem.as_str()) {
        s.insert(0, '_');
    }
    if s.chars().count() > 180 {
        let (stem, ext) = split_ext(&s);
        let keep: String = stem.chars().take(180 - ext.chars().count().min(20)).collect();
        s = format!("{keep}{ext}");
    }
    s
}

pub fn split_ext(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(i) if i > 0 && name.len() - i <= 12 => (&name[..i], &name[i..]),
        _ => (name, ""),
    }
}

pub fn has_ext(name: &str) -> bool {
    !split_ext(name).1.is_empty()
}

pub fn part_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".ddd-part");
    PathBuf::from(s)
}

pub fn unique(dir: &Path, name: &str, taken: &[PathBuf]) -> PathBuf {
    let (stem, ext) = split_ext(name);
    let mut n = 0;
    loop {
        let candidate = if n == 0 { name.to_string() } else { format!("{stem} ({n}){ext}") };
        let p = dir.join(&candidate);
        if !p.exists() && !part_path(&p).exists() && !taken.contains(&p) {
            return p;
        }
        n += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disposition() {
        assert_eq!(from_disposition("attachment; filename=\"a b.pdf\"").as_deref(), Some("a b.pdf"));
        assert_eq!(
            from_disposition("attachment; filename=\"fallback.pdf\"; filename*=UTF-8''%E2%82%AC%20rates.pdf").as_deref(),
            Some("€ rates.pdf")
        );
        assert_eq!(from_disposition("inline; filename=plain.txt").as_deref(), Some("plain.txt"));
        assert_eq!(from_disposition("attachment; filename=\"semi;colon.zip\"").as_deref(), Some("semi;colon.zip"));
        assert_eq!(from_disposition("attachment"), None);
    }

    #[test]
    fn url_names() {
        assert_eq!(from_url("https://x.org/files/My%20Book.epub?x=1").as_deref(), Some("My Book.epub"));
        assert_eq!(from_url("https://x.org/"), None);
    }

    #[test]
    fn cleaning() {
        assert_eq!(sanitize("a/b:c?.txt"), "a_b_c_.txt");
        assert_eq!(sanitize("CON.txt"), "_CON.txt");
        assert_eq!(sanitize("  name.  "), "name");
        assert_eq!(sanitize(""), "download");
        assert!(sanitize(&"x".repeat(400)).chars().count() <= 180);
    }

    #[test]
    fn unique_names() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.zip"), b"").unwrap();
        assert_eq!(unique(dir.path(), "a.zip", &[]), dir.path().join("a (1).zip"));
        assert_eq!(unique(dir.path(), "b.zip", &[dir.path().join("b.zip")]), dir.path().join("b (1).zip"));
    }
}
