use std::path::Path;

pub fn mark(path: &Path, url: &str, referrer: Option<&str>) {
    if !cfg!(windows) || !(url.starts_with("http://") || url.starts_with("https://") || url.starts_with("ftp://")) {
        return;
    }
    let mut body = String::from("[ZoneTransfer]\r\nZoneId=3\r\n");
    if let Some(r) = referrer.filter(|r| !r.is_empty()) {
        body.push_str(&format!("ReferrerUrl={r}\r\n"));
    }
    body.push_str(&format!("HostUrl={url}\r\n"));
    let mut ads = path.as_os_str().to_owned();
    ads.push(":Zone.Identifier");
    let _ = std::fs::write(ads, body);
}
