use crate::engine::{Fail, Job};
use serde::Serialize;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering::Relaxed;
use tokio::io::{AsyncBufReadExt, BufReader};

#[derive(Serialize, Clone, Debug)]
pub struct Info {
    pub title: String,
    pub thumb: Option<String>,
    pub heights: Vec<u32>,
    pub duration: Option<f64>,
    pub site: String,
}

#[derive(Clone, Debug)]
pub struct Fmt {
    pub id: String,
    pub url: String,
    pub protocol: String,
    pub ext: String,
    pub headers: Vec<(String, String)>,
    pub height: u32,
    pub has_audio: bool,
}

pub struct Pick {
    pub video: Option<Fmt>,
    pub audio: Option<Fmt>,
    pub ext: String,
}

pub async fn probe(exe: &Path, url: &str, headers: &[(String, String)], token: &tokio_util::sync::CancellationToken) -> Result<Value, Fail> {
    let mut cmd = tokio::process::Command::new(exe);
    cmd.args(["-J", "--no-playlist", "--no-warnings", "--no-progress"]);
    for (k, v) in headers {
        cmd.arg("--add-header").arg(format!("{k}:{v}"));
    }
    cmd.arg("--").arg(url).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
    let child = crate::proc::spawn(&mut cmd).map_err(|e| Fail::Other(format!("Couldn't start yt-dlp: {e}")))?;
    let out = tokio::select! {
        o = child.wait_with_output() => o.map_err(|e| Fail::Other(e.to_string()))?,
        _ = token.cancelled() => return Err(Fail::Stopped),
    };
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let line = err.lines().rev().find(|l| l.contains("ERROR")).unwrap_or("").trim();
        let msg = line.trim_start_matches("ERROR:").trim();
        return Err(Fail::Other(if msg.contains("Unsupported URL") || msg.is_empty() {
            "No video found on that page".into()
        } else {
            msg.chars().take(160).collect()
        }));
    }
    serde_json::from_slice(&out.stdout).map_err(|_| Fail::Other("yt-dlp gave an answer that couldn't be read".into()))
}

fn s(v: &Value, k: &str) -> String {
    v.get(k).and_then(Value::as_str).unwrap_or("").to_string()
}

fn has(v: &Value, k: &str) -> bool {
    !matches!(v.get(k).and_then(Value::as_str), None | Some("none"))
}

pub fn info(v: &Value) -> Info {
    let mut heights: Vec<u32> = formats(v).iter().filter(|f| f.height > 0).map(|f| f.height).collect();
    heights.sort_unstable_by(|a, b| b.cmp(a));
    heights.dedup();
    Info {
        title: s(v, "title"),
        thumb: v.get("thumbnail").and_then(Value::as_str).map(str::to_string),
        heights,
        duration: v.get("duration").and_then(Value::as_f64),
        site: s(v, "extractor_key"),
    }
}

fn fmt(f: &Value) -> Fmt {
    Fmt {
        id: s(f, "format_id"),
        url: s(f, "url"),
        protocol: s(f, "protocol"),
        ext: s(f, "ext"),
        headers: f
            .get("http_headers")
            .and_then(Value::as_object)
            .map(|m| m.iter().filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string()))).collect())
            .unwrap_or_default(),
        height: f.get("height").and_then(Value::as_u64).unwrap_or(0) as u32,
        has_audio: has(f, "acodec"),
    }
}

fn formats(v: &Value) -> Vec<Fmt> {
    match v.get("formats").and_then(Value::as_array) {
        Some(a) => a
            .iter()
            .filter(|f| has(f, "vcodec") && s(f, "protocol") != "mhtml" && !s(f, "format_note").contains("storyboard"))
            .map(fmt)
            .collect(),
        None => vec![fmt(v)].into_iter().filter(|f| !f.url.is_empty()).collect(),
    }
}

fn native(p: &str) -> u8 {
    match p {
        "https" | "http" => 2,
        "m3u8_native" | "m3u8" => 1,
        _ => 0,
    }
}

fn rate(f: &Value) -> f64 {
    f.get("tbr").or(f.get("vbr")).or(f.get("abr")).and_then(Value::as_f64).unwrap_or(0.0)
}

pub fn pick(v: &Value, height: Option<u32>, audio_only: bool) -> Pick {
    let all = v.get("formats").and_then(Value::as_array).cloned().unwrap_or_default();
    let audio_of = |prefer: &str| {
        let mut a: Vec<&Value> = all.iter().filter(|f| has(f, "acodec") && !has(f, "vcodec")).collect();
        a.sort_by(|x, y| {
            let px = (s(x, "ext") == prefer) as u8;
            let py = (s(y, "ext") == prefer) as u8;
            (px, native(&s(x, "protocol")), rate(x)).partial_cmp(&(py, native(&s(y, "protocol")), rate(y))).unwrap_or(std::cmp::Ordering::Equal)
        });
        a.last().map(|f| fmt(f))
    };
    if audio_only {
        let a = audio_of("m4a");
        let ext = a.as_ref().map(|a| a.ext.clone()).unwrap_or_else(|| "m4a".into());
        return Pick { video: None, audio: a, ext };
    }
    if all.is_empty() {
        let f = fmt(v);
        return Pick { ext: if f.ext.is_empty() { "mp4".into() } else { f.ext.clone() }, video: Some(f), audio: None };
    }
    let mut vids: Vec<&Value> = all
        .iter()
        .filter(|f| has(f, "vcodec") && s(f, "protocol") != "mhtml")
        .filter(|f| height.is_none_or(|h| f.get("height").and_then(Value::as_u64).unwrap_or(0) as u32 <= h))
        .collect();
    vids.sort_by(|x, y| {
        let hx = x.get("height").and_then(Value::as_u64).unwrap_or(0);
        let hy = y.get("height").and_then(Value::as_u64).unwrap_or(0);
        let mx = (s(x, "ext") == "mp4") as u8;
        let my = (s(y, "ext") == "mp4") as u8;
        (hx, native(&s(x, "protocol")), mx, rate(x)).partial_cmp(&(hy, native(&s(y, "protocol")), my, rate(y))).unwrap_or(std::cmp::Ordering::Equal)
    });
    let video = vids.last().map(|f| fmt(f));
    let audio = match &video {
        Some(v) if v.has_audio => None,
        Some(v) => audio_of(if v.ext == "mp4" { "m4a" } else { "webm" }),
        None => audio_of("m4a"),
    };
    let ext = match (&video, &audio) {
        (Some(v), None) => v.ext.clone(),
        (Some(v), Some(a)) if v.ext == "mp4" && a.ext == "m4a" => "mp4".into(),
        (Some(v), Some(a)) if v.ext == "webm" && a.ext == "webm" => "webm".into(),
        (Some(_), Some(_)) => "mkv".into(),
        (None, Some(a)) => a.ext.clone(),
        (None, None) => "mp4".into(),
    };
    Pick { video, audio, ext }
}

pub fn native_ok(f: &Fmt) -> bool {
    native(&f.protocol) > 0 && !f.url.is_empty()
}

pub async fn run(
    job: &Job,
    exe: &Path,
    ffmpeg: Option<&Path>,
    url: &str,
    format: &str,
    out: &Path,
    headers: &[(String, String)],
) -> Result<PathBuf, Fail> {
    let mut cmd = tokio::process::Command::new(exe);
    cmd.args(["--no-playlist", "--no-warnings", "--newline", "-N", "8", "-f", format]);
    cmd.args(["--progress-template", "download:ddd %(progress.downloaded_bytes)s %(progress.total_bytes)s %(progress.total_bytes_estimate)s"]);
    cmd.args(["--print", "after_move:filepath"]);
    if let Some(ff) = ffmpeg.and_then(|f| f.parent()) {
        cmd.arg("--ffmpeg-location").arg(ff);
    }
    for (k, v) in headers {
        cmd.arg("--add-header").arg(format!("{k}:{v}"));
    }
    let tpl = format!("{}.%(ext)s", out.with_extension("").display());
    cmd.arg("-o").arg(tpl).arg("--").arg(url).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
    let mut child = crate::proc::spawn(&mut cmd).map_err(|e| Fail::Other(format!("Couldn't start yt-dlp: {e}")))?;
    let mut lines = BufReader::new(child.stdout.take().expect("piped")).lines();
    let mut stderr = child.stderr.take().expect("piped");
    let err_task = tokio::spawn(async move {
        let mut s = String::new();
        let _ = tokio::io::AsyncReadExt::read_to_string(&mut stderr, &mut s).await;
        s
    });
    let mut last_path = None;
    let mut base = 0u64;
    let mut prev = 0u64;
    loop {
        let line = tokio::select! {
            l = lines.next_line() => l.map_err(|e| Fail::Other(e.to_string()))?,
            _ = job.token.cancelled() => { let _ = child.kill().await; return Err(Fail::Stopped) }
        };
        let Some(line) = line else { break };
        if let Some(rest) = line.strip_prefix("ddd ") {
            let mut parts = rest.split_whitespace().map(|p| p.parse::<f64>().ok());
            let got = parts.next().flatten().unwrap_or(0.0) as u64;
            let total = parts.next().flatten().or(parts.next().flatten()).unwrap_or(0.0) as u64;
            if got < prev {
                base += prev;
            }
            prev = got;
            job.progress.done.store(base + got, Relaxed);
            if total > 0 {
                job.progress.size.store(base + total, Relaxed);
            }
        } else if !line.trim().is_empty() && Path::new(line.trim()).is_absolute() {
            last_path = Some(PathBuf::from(line.trim()));
        }
    }
    let status = child.wait().await.map_err(|e| Fail::Other(e.to_string()))?;
    let err = err_task.await.unwrap_or_default();
    if !status.success() {
        let msg = err.lines().rev().find(|l| l.contains("ERROR")).unwrap_or("yt-dlp couldn't save it").trim_start_matches("ERROR:").trim().to_string();
        return Err(Fail::Other(msg.chars().take(160).collect()));
    }
    last_path.filter(|p| p.exists()).ok_or_else(|| Fail::Other("yt-dlp finished but the file wasn't found".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Value {
        serde_json::json!({
            "title": "Clip", "extractor_key": "Youtube",
            "formats": [
                {"format_id": "140", "ext": "m4a", "vcodec": "none", "acodec": "mp4a", "abr": 128.0, "protocol": "https", "url": "https://a/140"},
                {"format_id": "251", "ext": "webm", "vcodec": "none", "acodec": "opus", "abr": 160.0, "protocol": "https", "url": "https://a/251"},
                {"format_id": "137", "ext": "mp4", "vcodec": "avc1", "acodec": "none", "height": 1080, "tbr": 4000.0, "protocol": "https", "url": "https://a/137"},
                {"format_id": "248", "ext": "webm", "vcodec": "vp9", "acodec": "none", "height": 1080, "tbr": 3000.0, "protocol": "https", "url": "https://a/248"},
                {"format_id": "22", "ext": "mp4", "vcodec": "avc1", "acodec": "mp4a", "height": 720, "tbr": 1500.0, "protocol": "https", "url": "https://a/22"},
                {"format_id": "sb0", "ext": "mhtml", "vcodec": "none", "acodec": "none", "protocol": "mhtml", "format_note": "storyboard"}
            ]
        })
    }

    #[test]
    fn picks_best_mp4_pair() {
        let p = pick(&sample(), None, false);
        assert_eq!(p.video.unwrap().id, "137");
        assert_eq!(p.audio.unwrap().id, "140");
        assert_eq!(p.ext, "mp4");
    }

    #[test]
    fn honours_height_and_muxed() {
        let p = pick(&sample(), Some(720), false);
        assert_eq!(p.video.unwrap().id, "22");
        assert!(p.audio.is_none());
        assert_eq!(p.ext, "mp4");
    }

    #[test]
    fn audio_only_and_info() {
        let p = pick(&sample(), None, true);
        assert_eq!(p.audio.unwrap().id, "140");
        assert_eq!(p.ext, "m4a");
        assert_eq!(info(&sample()).heights, vec![1080, 720]);
    }
}
