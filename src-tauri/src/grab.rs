use serde::Serialize;

#[derive(Serialize, Clone, Copy, PartialEq, Eq, Debug, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum Group {
    Video,
    Audio,
    Archive,
    Document,
    Image,
    App,
    Torrent,
    Other,
}

#[derive(Serialize, Clone, Debug)]
pub struct Link {
    pub url: String,
    pub name: String,
    pub group: Group,
}

pub fn group_of(url: &str) -> Option<Group> {
    if url.starts_with("magnet:") {
        return Some(Group::Torrent);
    }
    let path = url::Url::parse(url).ok()?.path().to_ascii_lowercase();
    let ext = path.rsplit('/').next()?.rsplit_once('.')?.1.to_string();
    Some(match ext.as_str() {
        "mp4" | "mkv" | "webm" | "mov" | "avi" | "m4v" | "wmv" | "flv" | "ts" | "m3u8" | "mpd" => Group::Video,
        "mp3" | "flac" | "wav" | "m4a" | "aac" | "ogg" | "opus" | "wma" => Group::Audio,
        "zip" | "rar" | "7z" | "tar" | "gz" | "tgz" | "xz" | "bz2" | "zst" | "iso" | "img" | "dmg" => Group::Archive,
        "pdf" | "epub" | "mobi" | "azw3" | "djvu" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "odt" | "txt" | "csv" | "cbz" | "cbr" | "fb2" => Group::Document,
        "jpg" | "jpeg" | "png" | "gif" | "webp" | "avif" | "bmp" | "tif" | "tiff" | "svg" | "heic" | "raw" | "psd" => Group::Image,
        "exe" | "msi" | "msix" | "apk" | "appimage" | "deb" | "rpm" | "pkg" | "jar" | "bin" => Group::App,
        "torrent" => Group::Torrent,
        "json" | "xml" | "srt" | "vtt" | "ass" | "ttf" | "otf" | "woff2" => Group::Other,
        _ => return None,
    })
}

fn decode_entities(s: &str) -> String {
    s.replace("&amp;", "&").replace("&#38;", "&").replace("&quot;", "\"").replace("&#39;", "'").replace("&lt;", "<").replace("&gt;", ">")
}

fn candidates(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let lower = text.to_ascii_lowercase();
    for attr in ["href=", "src=", "data-src=", "data-href=", "data-url=", "content="] {
        let mut from = 0;
        while let Some(i) = lower[from..].find(attr) {
            let start = from + i + attr.len();
            from = start;
            let rest = &text[start..];
            let (q, body) = match rest.chars().next() {
                Some(c @ ('"' | '\'')) => (Some(c), &rest[1..]),
                _ => (None, rest),
            };
            let end = match q {
                Some(c) => body.find(c),
                None => body.find(|c: char| c.is_whitespace() || c == '>'),
            }
            .unwrap_or(body.len().min(2048));
            out.push(decode_entities(body[..end].trim()));
        }
    }
    for scheme in ["https://", "http://", "ftp://", "magnet:?"] {
        let mut from = 0;
        while let Some(i) = text[from..].find(scheme) {
            let start = from + i;
            let end = text[start..]
                .find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '>' | '`' | ')' | ']'))
                .map(|e| start + e)
                .unwrap_or(text.len());
            out.push(decode_entities(text[start..end].trim_end_matches([',', '.', ';'])));
            from = end.max(start + scheme.len());
        }
    }
    out
}

pub fn extract(text: &str, base: Option<&str>) -> Vec<Link> {
    let base = base.and_then(|b| url::Url::parse(b).ok());
    let mut seen = std::collections::HashSet::new();
    let mut links = Vec::new();
    for c in candidates(text) {
        if c.is_empty() || c.starts_with('#') || c.starts_with("javascript:") || c.starts_with("mailto:") || c.starts_with("data:") {
            continue;
        }
        let url = if c.starts_with("magnet:") {
            c
        } else {
            match url::Url::parse(&c).or_else(|e| base.as_ref().ok_or(e).and_then(|b| b.join(&c))) {
                Ok(mut u) if matches!(u.scheme(), "http" | "https" | "ftp") => {
                    u.set_fragment(None);
                    u.to_string()
                }
                _ => continue,
            }
        };
        let Some(group) = group_of(&url) else { continue };
        if !seen.insert(url.clone()) {
            continue;
        }
        let name = if url.starts_with("magnet:") {
            url::Url::parse(&url)
                .ok()
                .and_then(|u| u.query_pairs().find(|(k, _)| k == "dn").map(|(_, v)| v.into_owned()))
                .unwrap_or_else(|| "Torrent".into())
        } else {
            crate::filename::from_url(&url).unwrap_or_else(|| url.clone())
        };
        links.push(Link { url, name, group });
    }
    links
}

pub fn urls_in(text: &str) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    candidates(text)
        .into_iter()
        .filter(|u| u.starts_with("http://") || u.starts_with("https://") || u.starts_with("ftp://") || u.starts_with("magnet:"))
        .filter(|u| seen.insert(u.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pulls_files_from_html() {
        let html = r##"<a href="/files/book.pdf">Book</a> <a href='https://cdn.x.com/a.zip?sig=1&amp;e=2'>zip</a>
            <img src="pic.jpg"> <a href="/about">About</a> <a href="#top">top</a>
            <a href="magnet:?xt=urn:btih:abc&dn=Ubuntu+ISO">m</a> plain https://y.org/song.mp3, done"##;
        let links = extract(html, Some("https://site.com/dir/page.html"));
        let urls: Vec<_> = links.iter().map(|l| l.url.as_str()).collect();
        assert!(urls.contains(&"https://site.com/files/book.pdf"));
        assert!(urls.contains(&"https://cdn.x.com/a.zip?sig=1&e=2"));
        assert!(urls.contains(&"https://site.com/dir/pic.jpg"));
        assert!(urls.contains(&"https://y.org/song.mp3"));
        assert!(!urls.iter().any(|u| u.contains("about")));
        let m = links.iter().find(|l| l.group == Group::Torrent).unwrap();
        assert_eq!(m.name, "Ubuntu ISO");
    }

    #[test]
    fn text_urls() {
        let t = "grab https://a.com/1.zip and https://a.com/2.zip\nhttps://a.com/1.zip";
        assert_eq!(urls_in(t).len(), 2);
    }
}
