use crate::engine::{self, Fail, Job};
use sha2::Digest;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering::Relaxed;
use tokio::sync::Mutex;

const YTDLP: &str = "https://github.com/yt-dlp/yt-dlp/releases/latest/download/";
const FFMPEG: &str = "https://github.com/yt-dlp/FFmpeg-Builds/releases/latest/download/";
const FFMPEG_ZIP: &str = "ffmpeg-master-latest-win64-gpl-shared.zip";

static FF_LOCK: Mutex<()> = Mutex::const_new(());
static YT_LOCK: Mutex<()> = Mutex::const_new(());
static YT_UPDATED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn on_path(exe: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|p| std::env::split_paths(&p).map(|d| d.join(exe)).find(|p| p.is_file()))
}

pub fn ffmpeg_ready(dir: &Path) -> Option<PathBuf> {
    let local = dir.join("ffmpeg").join("ffmpeg.exe");
    if local.is_file() {
        return Some(local);
    }
    on_path("ffmpeg.exe")
}

pub fn ytdlp_ready(dir: &Path) -> Option<PathBuf> {
    let local = dir.join("yt-dlp.exe");
    local.is_file().then_some(local)
}

async fn sums(job: &Job, url: &str) -> Result<String, Fail> {
    let r = job.net.client.get(url).send().await.map_err(Fail::net)?;
    if !r.status().is_success() {
        return Err(Fail::status(r.status()));
    }
    r.text().await.map_err(Fail::net)
}

fn expected(sums: &str, file: &str) -> Option<String> {
    sums.lines().find_map(|l| {
        let mut parts = l.split_whitespace();
        let hash = parts.next()?;
        let name = parts.next()?.trim_start_matches('*');
        (name == file).then(|| hash.to_ascii_lowercase())
    })
}

fn sha256_file(path: &Path) -> std::io::Result<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut h = sha2::Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex::encode(h.finalize()))
}

async fn checked(job: &Job, base: &str, file: &str, sums_name: &str, to: &Path) -> Result<(), Fail> {
    let sums_url = format!("{base}{sums_name}");
    let list = engine::retry(&job.token, || sums(job, &sums_url)).await?;
    let want = expected(&list, file).ok_or_else(|| Fail::Other(format!("Couldn't find a checksum for {file}")))?;
    engine::to_file(job, &format!("{base}{file}"), &[], to).await?;
    let p = to.to_path_buf();
    let got = tokio::task::spawn_blocking(move || sha256_file(&p))
        .await
        .map_err(|e| Fail::Other(e.to_string()))?
        .map_err(|e| Fail::Disk(e.to_string()))?;
    if got != want {
        let _ = std::fs::remove_file(to);
        return Err(Fail::Other(format!("{file} didn't match its published checksum")));
    }
    Ok(())
}

pub async fn ffmpeg(job: &Job, dir: &Path) -> Result<PathBuf, Fail> {
    if let Some(p) = ffmpeg_ready(dir) {
        return Ok(p);
    }
    let _g = FF_LOCK.lock().await;
    if let Some(p) = ffmpeg_ready(dir) {
        return Ok(p);
    }
    job.progress.preparing.store(true, Relaxed);
    std::fs::create_dir_all(dir).map_err(|e| Fail::Disk(e.to_string()))?;
    let zip = dir.join(FFMPEG_ZIP);
    let r = checked(job, FFMPEG, FFMPEG_ZIP, "checksums.sha256", &zip).await;
    job.progress.preparing.store(false, Relaxed);
    r?;
    let out = dir.join("ffmpeg");
    let z = zip.clone();
    let o = out.clone();
    tokio::task::spawn_blocking(move || unpack_bin(&z, &o))
        .await
        .map_err(|e| Fail::Other(e.to_string()))?
        .map_err(|e| Fail::Disk(e.to_string()))?;
    let _ = std::fs::remove_file(&zip);
    ffmpeg_ready(dir).ok_or_else(|| Fail::Other("FFmpeg didn't unpack properly".into()))
}

fn unpack_bin(zip: &Path, out: &Path) -> std::io::Result<()> {
    let mut a = zip::ZipArchive::new(std::fs::File::open(zip)?).map_err(std::io::Error::other)?;
    let tmp = out.with_extension("tmp");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp)?;
    for i in 0..a.len() {
        let mut f = a.by_index(i).map_err(std::io::Error::other)?;
        let name = f.name().replace('\\', "/");
        let Some(file) = name.rsplit_once("/bin/").map(|(_, f)| f.to_string()) else { continue };
        let keep = file == "ffmpeg.exe" || file.ends_with(".dll");
        if !keep || file.contains('/') {
            continue;
        }
        let mut w = std::fs::File::create(tmp.join(&file))?;
        std::io::copy(&mut f, &mut w)?;
    }
    let _ = std::fs::remove_dir_all(out);
    std::fs::rename(&tmp, out)
}

pub async fn ytdlp(job: &Job, dir: &Path) -> Result<PathBuf, Fail> {
    let _g = YT_LOCK.lock().await;
    if let Some(p) = ytdlp_ready(dir) {
        if !YT_UPDATED.swap(true, Relaxed) {
            let p2 = p.clone();
            tokio::spawn(async move {
                let mut c = tokio::process::Command::new(&p2);
                c.arg("-U").stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
                crate::proc::hide(&mut c);
                let _ = c.status().await;
            });
        }
        return Ok(p);
    }
    job.progress.preparing.store(true, Relaxed);
    std::fs::create_dir_all(dir).map_err(|e| Fail::Disk(e.to_string()))?;
    let tmp = dir.join("yt-dlp.exe.download");
    let r = checked(job, YTDLP, "yt-dlp.exe", "SHA2-256SUMS", &tmp).await;
    job.progress.preparing.store(false, Relaxed);
    r?;
    let p = dir.join("yt-dlp.exe");
    std::fs::rename(&tmp, &p).map_err(|e| Fail::Disk(e.to_string()))?;
    YT_UPDATED.store(true, Relaxed);
    Ok(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_checksums() {
        let s = "abc123  ffmpeg.zip\nDEF456 *yt-dlp.exe\n";
        assert_eq!(expected(s, "yt-dlp.exe").as_deref(), Some("def456"));
        assert_eq!(expected(s, "ffmpeg.zip").as_deref(), Some("abc123"));
        assert_eq!(expected(s, "nope"), None);
    }
}
