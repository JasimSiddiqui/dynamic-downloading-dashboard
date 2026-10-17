pub mod ftp;
pub mod hosts;
pub mod probe;
pub mod splitter;
pub mod writer;

use crate::store::Resume;
use crate::{filename, motw};
use bytes::BytesMut;
use futures_util::StreamExt;
use parking_lot::Mutex;
use probe::Head;
use reqwest::header::{HeaderMap, ACCEPT_ENCODING, RANGE};
use reqwest::{Response, StatusCode};
use splitter::{Plan, Seg};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::*};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

const FLUSH: usize = 512 * 1024;
const START: u32 = 4;

#[derive(Debug, Clone)]
pub enum Fail {
    Stopped,
    Page(String),
    Expired(u16),
    Changed,
    Throttled,
    Net(String),
    Http(u16),
    Disk(String),
    Other(String),
}

impl Fail {
    pub fn net(e: reqwest::Error) -> Self {
        if let Some(s) = e.status() {
            return Self::status(s);
        }
        Fail::Net(e.to_string())
    }

    pub fn status(s: StatusCode) -> Self {
        match s.as_u16() {
            401 | 403 | 404 | 410 => Fail::Expired(s.as_u16()),
            416 => Fail::Changed,
            429 | 503 | 509 => Fail::Throttled,
            c => Fail::Http(c),
        }
    }

    pub fn message(&self) -> String {
        match self {
            Fail::Stopped => "Stopped".into(),
            Fail::Page(_) => "That link opens a web page, not a file".into(),
            Fail::Expired(404) => "The server says this file isn't there anymore".into(),
            Fail::Expired(_) => "This link has expired or needs a login".into(),
            Fail::Changed => "The file changed on the server".into(),
            Fail::Throttled => "The server is busy".into(),
            Fail::Net(_) => "Lost the connection".into(),
            Fail::Http(c) => format!("The server answered with error {c}"),
            Fail::Disk(e) => format!("Couldn't save the file: {e}"),
            Fail::Other(e) => e.clone(),
        }
    }

    fn retryable(&self) -> bool {
        matches!(self, Fail::Net(_) | Fail::Http(500..=599) | Fail::Throttled)
    }
}

pub struct Limiter {
    rate: AtomicU64,
    bucket: Mutex<(Instant, f64)>,
}

impl Limiter {
    pub fn new(rate: u64) -> Self {
        Self { rate: AtomicU64::new(rate), bucket: Mutex::new((Instant::now(), 0.0)) }
    }

    pub fn set(&self, rate: u64) {
        self.rate.store(rate, Relaxed);
    }

    pub async fn take(&self, n: usize) {
        let rate = self.rate.load(Relaxed) as f64;
        if rate <= 0.0 {
            return;
        }
        let wait = {
            let mut b = self.bucket.lock();
            let now = Instant::now();
            let tokens = (b.1 + now.duration_since(b.0).as_secs_f64() * rate).min(rate / 4.0) - n as f64;
            *b = (now, tokens);
            if tokens < 0.0 {
                Duration::from_secs_f64(-tokens / rate)
            } else {
                Duration::ZERO
            }
        };
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
    }
}

pub struct Net {
    pub client: reqwest::Client,
    pub limiter: Limiter,
    pub hosts: hosts::Hosts,
}

pub fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .http1_only()
        .user_agent(probe::UA)
        .connect_timeout(Duration::from_secs(15))
        .read_timeout(Duration::from_secs(30))
        .pool_max_idle_per_host(64)
        .tcp_nodelay(true)
        .redirect(reqwest::redirect::Policy::limited(12))
        .build()
        .expect("http client")
}

#[derive(Default)]
pub struct Progress {
    pub done: AtomicU64,
    pub size: AtomicU64,
    pub lanes: Mutex<Vec<[f32; 3]>>,
    pub waiting: AtomicBool,
    pub preparing: AtomicBool,
}

pub struct Job {
    pub net: Arc<Net>,
    pub token: CancellationToken,
    pub progress: Arc<Progress>,
    pub conns: u32,
}

pub trait Hooks: Send + Sync {
    fn named(&self, name: &str, path: &Path, size: Option<u64>);
    fn save(&self, resume: Resume);
}

pub struct Target {
    pub sources: Vec<String>,
    pub headers: Vec<(String, String)>,
    pub dir: PathBuf,
    pub name: Option<String>,
    pub path: Option<PathBuf>,
    pub resume: Option<Resume>,
    pub allow_page: bool,
    pub mark: bool,
}

pub struct Done {
    pub path: PathBuf,
    pub size: u64,
}

pub async fn fetch(job: &Job, t: &Target, hooks: &dyn Hooks) -> Result<Done, Fail> {
    let headers = probe::header_map(&t.headers);
    let first = t.sources.first().ok_or_else(|| Fail::Other("No link".into()))?;
    let (head, resp) = retry(&job.token, || probe::open(&job.net.client, first, &headers)).await?;
    if head.is_page() && !t.allow_page && t.resume.is_none() && t.path.is_none() {
        return Err(Fail::Page(head.url.clone()));
    }
    let path = match &t.path {
        Some(p) => p.clone(),
        None => {
            let name = t.name.clone().map(|n| filename::sanitize(&n)).unwrap_or_else(|| head.file_name(first));
            let p = filename::unique(&t.dir, &name, &[]);
            hooks.named(&p.file_name().unwrap_or_default().to_string_lossy(), &p, head.size);
            p
        }
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| Fail::Disk(e.to_string()))?;
    }
    let part = filename::part_path(&path);
    job.progress.size.store(head.size.unwrap_or(0), Relaxed);
    let size = match (head.ranges, head.size) {
        (true, Some(size)) if size > 0 => {
            ranged(job, t, &headers, &head, resp, &part, size, hooks).await?;
            size
        }
        _ => single(job, resp, &part).await?,
    };
    finish(&part, &path, size, &head, t).await?;
    Ok(Done { path, size })
}

async fn finish(part: &Path, path: &Path, size: u64, head: &Head, t: &Target) -> Result<(), Fail> {
    let len = std::fs::metadata(part).map_err(|e| Fail::Disk(e.to_string()))?.len();
    if len != size {
        return Err(Fail::Other("The download came out the wrong size".into()));
    }
    if let Some(d) = head.digest.clone() {
        let p = part.to_path_buf();
        let ok = tokio::task::spawn_blocking(move || probe::verify(&p, &d)).await.ok().and_then(|r| r.ok());
        if ok == Some(false) {
            let _ = std::fs::remove_file(part);
            return Err(Fail::Other("The file didn't match the server's checksum, so it was thrown away".into()));
        }
    }
    let target = if path.exists() { filename::unique(path.parent().unwrap_or(Path::new(".")), &path.file_name().unwrap_or_default().to_string_lossy(), &[]) } else { path.to_path_buf() };
    std::fs::rename(part, &target).map_err(|e| Fail::Disk(e.to_string()))?;
    if t.mark {
        let referrer = t.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("referer")).map(|(_, v)| v.as_str());
        motw::mark(&target, &head.url, referrer);
    }
    Ok(())
}

pub async fn retry<T, F, Fut>(token: &CancellationToken, mut f: F) -> Result<T, Fail>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, Fail>>,
{
    let mut attempt = 0u32;
    loop {
        let r = tokio::select! {
            r = f() => r,
            _ = token.cancelled() => return Err(Fail::Stopped),
        };
        match r {
            Err(e) if e.retryable() && attempt < 5 => {
                attempt += 1;
                if sleep(token, backoff(attempt)).await {
                    return Err(Fail::Stopped);
                }
            }
            r => return r,
        }
    }
}

fn backoff(attempt: u32) -> Duration {
    Duration::from_millis((500u64 << attempt.min(5)).min(15_000))
}

async fn sleep(token: &CancellationToken, d: Duration) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(d) => false,
        _ = token.cancelled() => true,
    }
}

async fn single(job: &Job, resp: Response, part: &Path) -> Result<u64, Fail> {
    let w = writer::Writer::open(part, None, true).map_err(|e| Fail::Disk(e.to_string()))?;
    let plan = Plan::new(u64::MAX);
    let s = plan.claim().ok_or_else(|| Fail::Other("No work".into()))?;
    let tx = w.tx();
    let mut body = resp.bytes_stream();
    let mut buf = BytesMut::with_capacity(FLUSH);
    let mut off = 0u64;
    let mut total = 0u64;
    loop {
        let next = tokio::select! {
            n = body.next() => n,
            _ = job.token.cancelled() => { drop(tx); let _ = w.finish().await; return Err(Fail::Stopped) }
        };
        match next {
            None => break,
            Some(Err(e)) => {
                drop(tx);
                let _ = w.finish().await;
                return Err(Fail::net(e));
            }
            Some(Ok(chunk)) => {
                job.net.limiter.take(chunk.len()).await;
                buf.extend_from_slice(&chunk);
                total += chunk.len() as u64;
                job.progress.done.store(total, Relaxed);
                if buf.len() >= FLUSH {
                    let data = buf.split().freeze();
                    let n = data.len() as u64;
                    tx.send(writer::Chunk { seg: s.clone(), off, data }).await.map_err(|_| disk_gone())?;
                    off += n;
                }
            }
        }
    }
    if !buf.is_empty() {
        tx.send(writer::Chunk { seg: s.clone(), off, data: buf.freeze() }).await.map_err(|_| disk_gone())?;
    }
    drop(tx);
    w.finish().await.map_err(|e| Fail::Disk(e.to_string()))?;
    Ok(total)
}

fn disk_gone() -> Fail {
    Fail::Disk("the disk stopped accepting writes".into())
}

struct Source {
    url: String,
    workers: AtomicU32,
    dead: AtomicBool,
    bytes: AtomicU64,
}

#[derive(Default)]
struct Lane {
    kick: AtomicBool,
    bytes: AtomicU64,
}

struct Swarm {
    plan: Arc<Plan>,
    sources: Vec<Source>,
    headers: HeaderMap,
    tx: writer::Tx,
    net: Arc<Net>,
    token: CancellationToken,
    etag: Option<String>,
    size: u64,
    throttled: AtomicU32,
    active: AtomicU32,
    backing_off: AtomicU32,
}

impl Swarm {
    fn source(&self) -> Option<&Source> {
        self.sources
            .iter()
            .filter(|s| !s.dead.load(Relaxed))
            .min_by_key(|s| (s.workers.load(Relaxed), u64::MAX - s.bytes.load(Relaxed)))
    }

    async fn request(&self, src: &Source, seg: &Seg) -> Result<Response, Fail> {
        let pos = seg.pos.load(Acquire);
        let end = seg.end.load(Acquire);
        let resp = self
            .net
            .client
            .get(&src.url)
            .headers(self.headers.clone())
            .header(RANGE, format!("bytes={pos}-{}", end - 1))
            .header(ACCEPT_ENCODING, "identity")
            .send()
            .await
            .map_err(Fail::net)?;
        let status = resp.status();
        if !status.is_success() {
            return Err(Fail::status(status));
        }
        if status != StatusCode::PARTIAL_CONTENT {
            return if pos == 0 { Ok(resp) } else { Err(Fail::Other("This mirror doesn't support splitting".into())) };
        }
        let cr = resp.headers().get("content-range").and_then(|v| v.to_str().ok()).unwrap_or("");
        let start = cr.trim_start_matches("bytes").trim().split('-').next().and_then(|s| s.parse::<u64>().ok());
        let total = cr.rsplit('/').next().and_then(|s| s.trim().parse::<u64>().ok());
        if start != Some(pos) || total.is_some_and(|t| t != self.size) {
            return Err(Fail::Changed);
        }
        if let (Some(want), Some(got)) = (&self.etag, resp.headers().get("etag").and_then(|v| v.to_str().ok())) {
            if !got.starts_with("W/") && got != want && self.sources.len() == 1 {
                return Err(Fail::Changed);
            }
        }
        Ok(resp)
    }

    async fn stream(&self, resp: Response, seg: &Arc<Seg>, lane: &Lane, src: &Source) -> Result<(), Fail> {
        let mut body = resp.bytes_stream();
        let mut buf = BytesMut::with_capacity(FLUSH);
        let mut off = seg.pos.load(Acquire);
        let result = loop {
            let next = tokio::select! {
                n = body.next() => n,
                _ = self.token.cancelled() => break Err(Fail::Stopped),
            };
            let chunk = match next {
                None => break Ok(()),
                Some(Err(e)) => break Err(Fail::net(e)),
                Some(Ok(c)) => c,
            };
            self.net.limiter.take(chunk.len()).await;
            let pos = seg.pos.load(Acquire);
            let end = seg.end.load(Acquire);
            if pos >= end {
                break Ok(());
            }
            let take = (chunk.len() as u64).min(end - pos) as usize;
            if buf.is_empty() {
                off = pos;
            }
            buf.extend_from_slice(&chunk[..take]);
            seg.advance(pos + take as u64);
            lane.bytes.fetch_add(take as u64, Relaxed);
            src.bytes.fetch_add(take as u64, Relaxed);
            if buf.len() >= FLUSH {
                let data = buf.split().freeze();
                let n = data.len() as u64;
                if self.tx.send(writer::Chunk { seg: seg.clone(), off, data }).await.is_err() {
                    return Err(disk_gone());
                }
                off += n;
            }
            if take < chunk.len() || pos + take as u64 >= end {
                break Ok(());
            }
            if lane.kick.swap(false, AcqRel) {
                break Ok(());
            }
        };
        if !buf.is_empty() && self.tx.send(writer::Chunk { seg: seg.clone(), off, data: buf.freeze() }).await.is_err() {
            return Err(disk_gone());
        }
        result
    }

    async fn drain(&self, seg: &Arc<Seg>, lane: &Lane, mut first: Option<Response>) -> Result<(), Fail> {
        let mut attempt = 0u32;
        loop {
            if seg.left() == 0 {
                return Ok(());
            }
            if self.token.is_cancelled() {
                return Err(Fail::Stopped);
            }
            let Some(src) = self.source() else {
                return Err(Fail::Other("Every source for this file stopped working".into()));
            };
            src.workers.fetch_add(1, Relaxed);
            let r = match first.take().filter(|_| seg.pos.load(Acquire) == 0) {
                Some(resp) => Ok(resp),
                None => self.request(src, seg).await,
            };
            let r = match r {
                Ok(resp) => {
                    let before = seg.pos.load(Acquire);
                    let r = self.stream(resp, seg, lane, src).await;
                    if seg.pos.load(Acquire) > before {
                        attempt = 0;
                    }
                    r
                }
                Err(e) => Err(e),
            };
            src.workers.fetch_sub(1, Relaxed);
            match r {
                Ok(()) => continue,
                Err(Fail::Stopped) => return Err(Fail::Stopped),
                Err(Fail::Throttled) => {
                    self.throttled.fetch_add(1, AcqRel);
                    if self.active.load(Acquire) > 1 {
                        return Err(Fail::Throttled);
                    }
                }
                Err(e @ (Fail::Changed | Fail::Disk(_))) => return Err(e),
                Err(e @ (Fail::Expired(_) | Fail::Other(_))) => {
                    if self.sources.iter().filter(|s| !s.dead.load(Relaxed)).count() > 1 {
                        src.dead.store(true, Release);
                        continue;
                    }
                    return Err(e);
                }
                Err(Fail::Http(c)) if attempt >= 6 => return Err(Fail::Http(c)),
                Err(_) => {}
            }
            attempt += 1;
            self.backing_off.fetch_add(1, Relaxed);
            let stopped = sleep(&self.token, backoff(attempt)).await;
            self.backing_off.fetch_sub(1, Relaxed);
            if stopped {
                return Err(Fail::Stopped);
            }
        }
    }
}

async fn worker(sw: Arc<Swarm>, lane: Arc<Lane>, mut first: Option<Response>) -> Result<(), Fail> {
    sw.active.fetch_add(1, AcqRel);
    let r = async {
        while let Some(seg) = sw.plan.claim() {
            let r = sw.drain(&seg, &lane, first.take()).await;
            sw.plan.release(&seg);
            r?;
        }
        Ok(())
    }
    .await;
    sw.active.fetch_sub(1, AcqRel);
    r
}

#[allow(clippy::too_many_arguments)]
async fn ranged(
    job: &Job,
    t: &Target,
    headers: &HeaderMap,
    head: &Head,
    resp: Response,
    part: &Path,
    size: u64,
    hooks: &dyn Hooks,
) -> Result<(), Fail> {
    let resumable = t.resume.as_ref().filter(|r| {
        r.size == size
            && (r.etag.is_none() || head.etag.is_none() || r.etag == head.etag)
            && std::fs::metadata(part).map(|m| m.len() == size).unwrap_or(false)
    });
    let plan = Arc::new(match resumable {
        Some(r) => Plan::from_holes(size, &r.holes),
        None => Plan::new(size),
    });
    let first = if resumable.is_none() { Some(resp) } else { drop(resp); None };
    let w = writer::Writer::open(part, Some(size), resumable.is_none()).map_err(|e| Fail::Disk(e.to_string()))?;
    let mut sources = vec![Source { url: head.url.clone(), workers: AtomicU32::new(0), dead: AtomicBool::new(false), bytes: AtomicU64::new(0) }];
    sources.extend(mirrors(job, &t.sources[1..], headers, size).await.into_iter().map(|url| Source {
        url,
        workers: AtomicU32::new(0),
        dead: AtomicBool::new(false),
        bytes: AtomicU64::new(0),
    }));
    let host = url::Url::parse(&head.url).ok().and_then(|u| u.host_str().map(str::to_string)).unwrap_or_default();
    let mut cap = job.conns.clamp(1, 32);
    if let Some(c) = job.net.hosts.cap(&host) {
        cap = cap.min(c.max(1) * sources.len() as u32);
    }
    let sw = Arc::new(Swarm {
        plan: plan.clone(),
        sources,
        headers: headers.clone(),
        tx: w.tx(),
        net: job.net.clone(),
        token: job.token.clone(),
        etag: head.etag.clone(),
        size,
        throttled: AtomicU32::new(0),
        active: AtomicU32::new(0),
        backing_off: AtomicU32::new(0),
    });
    let save = |plan: &Plan| {
        hooks.save(Resume { size, holes: plan.holes(), etag: head.etag.clone(), modified: head.modified.clone() })
    };

    let mut set: JoinSet<Result<(), Fail>> = JoinSet::new();
    let mut lanes: Vec<Stat> = Vec::new();
    let spawn = |set: &mut JoinSet<Result<(), Fail>>, lanes: &mut Vec<Stat>, first: Option<Response>| {
        let lane = Arc::new(Lane::default());
        lanes.push(Stat { lane: lane.clone(), born: Instant::now(), last: 0, rate: 0.0, tick: 0 });
        set.spawn(worker(sw.clone(), lane, first));
    };
    spawn(&mut set, &mut lanes, first);
    for _ in 1..START.min(cap) {
        spawn(&mut set, &mut lanes, None);
    }

    let mut tick = tokio::time::interval(Duration::from_millis(250));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut mark = (Instant::now(), plan.received());
    let mut before: Option<(f64, u32, u32)> = None;
    let mut stalls = 0u32;
    let mut ramping = true;
    let mut ticks = 0u64;
    let mut fatal: Option<Fail> = None;
    let mut flowing = [0u32; 8];

    while !set.is_empty() {
        tokio::select! {
            _ = tick.tick() => {
                ticks += 1;
                let received = plan.received();
                job.progress.done.store(received, Relaxed);
                *job.progress.lanes.lock() = plan.lanes();
                let active = sw.active.load(Acquire);
                job.progress.waiting.store(active > 0 && sw.backing_off.load(Relaxed) >= active, Relaxed);
                flowing[(ticks % 8) as usize] = lanes
                    .iter_mut()
                    .map(|l| {
                        let b = l.lane.bytes.load(Relaxed);
                        let moved = b > l.tick;
                        l.tick = b;
                        moved as u32
                    })
                    .sum();
                if sw.throttled.swap(0, AcqRel) > 0 {
                    cap = flowing.iter().copied().max().unwrap_or(1).max(1).min(cap);
                    job.net.hosts.limit(&host, cap);
                    ramping = false;
                }
                if ticks % 4 != 0 {
                    continue;
                }
                save(&plan);
                kick_slow(&mut lanes);
                let now = Instant::now();
                let rate = (received - mark.1) as f64 / now.duration_since(mark.0).as_secs_f64().max(0.001);
                mark = (now, received);
                let n = set.len() as u32;
                if let Some((prev, prev_n, added)) = before.take() {
                    let per = prev / prev_n.max(1) as f64;
                    if rate - prev >= per * added as f64 * 0.35 {
                        stalls = 0;
                    } else {
                        stalls += 1;
                        if stalls >= 2 {
                            ramping = false;
                        }
                    }
                }
                if !ramping && ticks % 60 == 0 {
                    ramping = true;
                    stalls = 1;
                }
                if ramping && n < cap && plan.splittable() && !job.progress.waiting.load(Relaxed) {
                    let add = n.clamp(1, 4).min(cap - n);
                    for _ in 0..add {
                        spawn(&mut set, &mut lanes, None);
                    }
                    before = Some((rate, n, add));
                }
            }
            r = set.join_next() => {
                match r {
                    Some(Ok(Err(Fail::Stopped))) | Some(Ok(Ok(()))) | Some(Ok(Err(Fail::Throttled))) | None => {}
                    Some(Ok(Err(e))) => {
                        if fatal.is_none() && !matches!(e, Fail::Net(_) | Fail::Http(_)) || set.is_empty() {
                            fatal = Some(e);
                            job.token.cancel();
                        }
                    }
                    Some(Err(e)) => {
                        fatal = Some(Fail::Other(e.to_string()));
                        job.token.cancel();
                    }
                }
                lanes.retain(|l| Arc::strong_count(&l.lane) > 1);
                if set.is_empty() && !plan.complete() && fatal.is_none() && !job.token.is_cancelled() && plan.splittable() {
                    spawn(&mut set, &mut lanes, None);
                }
            }
        }
    }
    job.net.hosts.raise(&host, cap);
    drop(sw);
    w.finish().await.map_err(|e| Fail::Disk(e.to_string()))?;
    save(&plan);
    *job.progress.lanes.lock() = Vec::new();
    job.progress.done.store(plan.received(), Relaxed);
    if let Some(e) = fatal {
        return Err(e);
    }
    if job.token.is_cancelled() {
        return Err(Fail::Stopped);
    }
    if !plan.complete() {
        return Err(Fail::Other("The download stopped before the end".into()));
    }
    Ok(())
}

struct Stat {
    lane: Arc<Lane>,
    born: Instant,
    last: u64,
    rate: f64,
    tick: u64,
}

fn kick_slow(lanes: &mut [Stat]) {
    let now = Instant::now();
    for l in lanes.iter_mut() {
        let bytes = l.lane.bytes.load(Relaxed);
        let inst = (bytes - l.last) as f64;
        l.rate = if l.rate == 0.0 { inst } else { l.rate * 0.6 + inst * 0.4 };
        l.last = bytes;
    }
    let mature: Vec<f64> = lanes.iter().filter(|l| now.duration_since(l.born) > Duration::from_secs(5)).map(|l| l.rate).collect();
    if mature.len() < 3 {
        return;
    }
    let mut sorted = mature.clone();
    sorted.sort_by(f64::total_cmp);
    let median = sorted[sorted.len() / 2];
    if median <= 0.0 {
        return;
    }
    for l in lanes.iter_mut() {
        if now.duration_since(l.born) > Duration::from_secs(5) && l.rate < median * 0.25 {
            l.lane.kick.store(true, Release);
            l.born = now;
            l.rate = 0.0;
        }
    }
}

async fn mirrors(job: &Job, urls: &[String], headers: &HeaderMap, size: u64) -> Vec<String> {
    let checks = urls.iter().map(|u| {
        let client = job.net.client.clone();
        let headers = headers.clone();
        let u = u.clone();
        async move {
            let h = tokio::time::timeout(Duration::from_secs(15), probe::head(&client, &u, &headers)).await.ok()?.ok()?;
            (h.ranges && h.size == Some(size)).then_some(h.url)
        }
    });
    futures_util::future::join_all(checks).await.into_iter().flatten().collect()
}

pub async fn to_file(job: &Job, url: &str, headers: &[(String, String)], path: &Path) -> Result<u64, Fail> {
    struct Quiet;
    impl Hooks for Quiet {
        fn named(&self, _: &str, _: &Path, _: Option<u64>) {}
        fn save(&self, _: Resume) {}
    }
    let t = Target {
        sources: vec![url.to_string()],
        headers: headers.to_vec(),
        dir: path.parent().unwrap_or(Path::new(".")).to_path_buf(),
        name: None,
        path: Some(path.to_path_buf()),
        resume: None,
        allow_page: true,
        mark: false,
    };
    let _ = std::fs::remove_file(path);
    fetch(job, &t, &Quiet).await.map(|d| d.size)
}
