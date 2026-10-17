pub mod dash;
pub mod hls;
pub mod ytdlp;

use crate::engine::{self, Fail, Job, Progress};
use aes::cipher::{block_padding::Pkcs7, BlockDecryptMut, KeyIvInit};
use bytes::Bytes;
use futures_util::StreamExt;
use reqwest::header::{HeaderMap, RANGE};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering::*};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncWriteExt;

#[derive(Clone, Debug, PartialEq)]
pub struct Key {
    pub uri: String,
    pub iv: [u8; 16],
}

#[derive(Clone, Debug)]
pub struct Piece {
    pub url: String,
    pub range: Option<(u64, u64)>,
    pub key: Option<Key>,
}

#[derive(Clone, Debug, Default)]
pub struct Track {
    pub init: Option<Piece>,
    pub pieces: Vec<Piece>,
    pub whole: Option<String>,
    pub ext: &'static str,
}

#[derive(Default)]
pub struct Tally {
    pub total: AtomicU64,
    pub done: AtomicU64,
    pub bytes: AtomicU64,
}

impl Tally {
    pub fn publish(&self, p: &Progress) {
        let bytes = self.bytes.load(Relaxed);
        let total = self.total.load(Relaxed);
        let done = self.done.load(Relaxed);
        p.done.store(bytes, Relaxed);
        if total > 0 && done > 0 {
            p.size.store(((bytes as f64) * total as f64 / done as f64) as u64, Relaxed);
        }
    }
}

pub fn decrypt(data: &[u8], key: &[u8; 16], iv: &[u8; 16]) -> Result<Vec<u8>, Fail> {
    cbc::Decryptor::<aes::Aes128>::new(key.into(), iv.into())
        .decrypt_padded_vec_mut::<Pkcs7>(data)
        .map_err(|_| Fail::Other("A piece of this stream couldn't be decrypted".into()))
}

pub fn resolve(base: &str, rel: &str) -> String {
    url::Url::parse(base).and_then(|b| b.join(rel)).map(|u| u.to_string()).unwrap_or_else(|_| rel.to_string())
}

pub async fn text(job: &Job, url: &str, headers: &HeaderMap) -> Result<(String, String), Fail> {
    engine::retry(&job.token, || async {
        let r = job.net.client.get(url).headers(headers.clone()).send().await.map_err(Fail::net)?;
        if !r.status().is_success() {
            return Err(Fail::status(r.status()));
        }
        let final_url = r.url().to_string();
        Ok((final_url, r.text().await.map_err(Fail::net)?))
    })
    .await
}

struct Keys {
    map: tokio::sync::Mutex<HashMap<String, [u8; 16]>>,
}

impl Keys {
    async fn get(&self, job: &Job, headers: &HeaderMap, uri: &str) -> Result<[u8; 16], Fail> {
        let mut map = self.map.lock().await;
        if let Some(k) = map.get(uri) {
            return Ok(*k);
        }
        let bytes = get_bytes(job, headers, uri, None).await?;
        let key: [u8; 16] = bytes.as_ref().try_into().map_err(|_| Fail::Other("This stream's key isn't a normal AES-128 key".into()))?;
        map.insert(uri.to_string(), key);
        Ok(key)
    }
}

async fn get_bytes(job: &Job, headers: &HeaderMap, url: &str, range: Option<(u64, u64)>) -> Result<Bytes, Fail> {
    engine::retry(&job.token, || async {
        let mut req = job.net.client.get(url).headers(headers.clone());
        if let Some((off, len)) = range {
            req = req.header(RANGE, format!("bytes={off}-{}", off + len - 1));
        }
        let r = req.send().await.map_err(Fail::net)?;
        if !r.status().is_success() {
            return Err(Fail::status(r.status()));
        }
        let b = r.bytes().await.map_err(Fail::net)?;
        job.net.limiter.take(b.len()).await;
        Ok(b)
    })
    .await
}

async fn piece(job: &Job, headers: &HeaderMap, keys: &Keys, p: &Piece) -> Result<Bytes, Fail> {
    let data = get_bytes(job, headers, &p.url, p.range).await?;
    match &p.key {
        None => Ok(data),
        Some(k) => {
            let key = keys.get(job, headers, &k.uri).await?;
            decrypt(&data, &key, &k.iv).map(Bytes::from)
        }
    }
}

pub async fn pull(
    job: &Job,
    headers: &HeaderMap,
    track: &Track,
    out: &Path,
    start: (usize, u64),
    tally: &Tally,
    save: &(dyn Fn(usize, u64) + Sync),
) -> Result<(), Fail> {
    if let Some(url) = &track.whole {
        return whole(job, headers, url, out, tally).await;
    }
    let mut f = tokio::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(out)
        .await
        .map_err(|e| Fail::Disk(e.to_string()))?;
    let (mut done, mut bytes) = if start.0 > 0 && f.metadata().await.map(|m| m.len() >= start.1).unwrap_or(false) { start } else { (0, 0) };
    f.set_len(bytes).await.map_err(|e| Fail::Disk(e.to_string()))?;
    tokio::io::AsyncSeekExt::seek(&mut f, std::io::SeekFrom::Start(bytes)).await.map_err(|e| Fail::Disk(e.to_string()))?;
    tally.done.fetch_add(done as u64, Relaxed);
    tally.bytes.fetch_add(bytes, Relaxed);
    let keys = Keys { map: tokio::sync::Mutex::new(HashMap::new()) };
    if done == 0 {
        if let Some(init) = &track.init {
            let b = piece(job, headers, &keys, init).await?;
            f.write_all(&b).await.map_err(|e| Fail::Disk(e.to_string()))?;
            bytes += b.len() as u64;
            tally.bytes.fetch_add(b.len() as u64, Relaxed);
        }
    }
    let conc = job.conns.clamp(4, 32) as usize;
    let keys = &keys;
    let mut stream = futures_util::stream::iter(track.pieces[done..].to_vec())
        .map(move |p: Piece| async move { piece(job, headers, keys, &p).await })
        .buffered(conc);
    loop {
        let next = tokio::select! {
            n = stream.next() => n,
            _ = job.token.cancelled() => { save(done, bytes); return Err(Fail::Stopped) }
        };
        let Some(r) = next else { break };
        let b = match r {
            Ok(b) => b,
            Err(e) => {
                let _ = f.flush().await;
                save(done, bytes);
                return Err(e);
            }
        };
        f.write_all(&b).await.map_err(|e| Fail::Disk(e.to_string()))?;
        done += 1;
        bytes += b.len() as u64;
        tally.done.fetch_add(1, Relaxed);
        tally.bytes.fetch_add(b.len() as u64, Relaxed);
        tally.publish(&job.progress);
        if done % 8 == 0 {
            let _ = f.flush().await;
            save(done, bytes);
        }
    }
    f.flush().await.map_err(|e| Fail::Disk(e.to_string()))?;
    save(done, bytes);
    Ok(())
}

async fn whole(job: &Job, headers: &HeaderMap, url: &str, out: &Path, tally: &Tally) -> Result<(), Fail> {
    let sub = Job { net: job.net.clone(), token: job.token.clone(), progress: Arc::new(Progress::default()), conns: job.conns };
    let pairs: Vec<(String, String)> = headers
        .iter()
        .filter_map(|(k, v)| v.to_str().ok().map(|v| (k.to_string(), v.to_string())))
        .collect();
    let base = tally.bytes.load(Relaxed);
    let fut = engine::to_file(&sub, url, &pairs, out);
    tokio::pin!(fut);
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    let r = loop {
        tokio::select! {
            r = &mut fut => break r,
            _ = tick.tick() => {
                let got = sub.progress.done.load(Relaxed);
                let size = sub.progress.size.load(Relaxed);
                tally.bytes.store(base + got, Relaxed);
                job.progress.done.store(base + got, Relaxed);
                if size > 0 {
                    job.progress.size.store(base.max(job.progress.size.load(Relaxed)).max(base + size), Relaxed);
                }
                *job.progress.lanes.lock() = sub.progress.lanes.lock().clone();
            }
        }
    };
    let n = r?;
    tally.bytes.store(base + n, Relaxed);
    tally.done.fetch_add(1, Relaxed);
    Ok(())
}

pub async fn mux(job: &Job, ffmpeg: &Path, inputs: &[PathBuf], out: &Path) -> Result<PathBuf, Fail> {
    let attempt = |out: PathBuf| async move {
        let mut cmd = tokio::process::Command::new(ffmpeg);
        cmd.args(["-y", "-hide_banner", "-loglevel", "error"]);
        for i in inputs {
            cmd.arg("-i").arg(i);
        }
        if inputs.len() > 1 {
            for i in 0..inputs.len() {
                cmd.args(["-map", &format!("{i}")]);
            }
        }
        cmd.args(["-c", "copy"]);
        if out.extension().is_some_and(|e| e == "mp4" || e == "m4a") {
            cmd.args(["-movflags", "+faststart"]);
        }
        cmd.arg(&out).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::piped());
        let child = crate::proc::spawn(&mut cmd).map_err(|e| Fail::Other(format!("Couldn't start FFmpeg: {e}")))?;
        let output = tokio::select! {
            o = child.wait_with_output() => o.map_err(|e| Fail::Other(e.to_string()))?,
            _ = job.token.cancelled() => return Err(Fail::Stopped),
        };
        if output.status.success() {
            Ok(out)
        } else {
            let _ = std::fs::remove_file(&out);
            Err(Fail::Other(String::from_utf8_lossy(&output.stderr).lines().last().unwrap_or("FFmpeg failed").to_string()))
        }
    };
    match attempt(out.to_path_buf()).await {
        Err(Fail::Other(_)) if out.extension().is_some_and(|e| e != "mkv") => attempt(out.with_extension("mkv")).await,
        r => r,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aes::cipher::BlockEncryptMut;

    #[test]
    fn aes_round_trip() {
        let key = [7u8; 16];
        let iv = [9u8; 16];
        let plain = b"segment bytes that are not a multiple of sixteen".to_vec();
        let enc = cbc::Encryptor::<aes::Aes128>::new(&key.into(), &iv.into()).encrypt_padded_vec_mut::<Pkcs7>(&plain);
        assert_eq!(decrypt(&enc, &key, &iv).unwrap(), plain);
        assert!(decrypt(&enc, &[1u8; 16], &iv).is_err() || decrypt(&enc, &[1u8; 16], &iv).unwrap() != plain);
    }

    #[test]
    fn resolves_relative() {
        assert_eq!(resolve("https://a.com/x/master.m3u8", "v/1.ts"), "https://a.com/x/v/1.ts");
        assert_eq!(resolve("https://a.com/x/master.m3u8", "https://b.com/k"), "https://b.com/k");
    }
}
