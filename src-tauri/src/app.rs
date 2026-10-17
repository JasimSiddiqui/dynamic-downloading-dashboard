use crate::clipboard::{self, Catch};
use crate::engine::{self, probe, Fail, Hooks, Job, Net, Progress, Target};
use crate::media::{self, dash, hls, ytdlp, Tally};
use crate::store::{self, Item, Kind, Media, Resume, Saved, Settings, Status, Torrent, View};
use crate::{curl, filename, grab, proc, tools, torrent};
use parking_lot::Mutex;
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::*};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager};
use tokio_util::sync::CancellationToken;

pub struct Live {
    token: CancellationToken,
    progress: Arc<Progress>,
    last: (Instant, u64),
    speed: f64,
}

enum Pending {
    Video { url: String, headers: Vec<(String, String)>, title: String },
    Stream { url: String, headers: Vec<(String, String)>, dash: bool, name: String },
    Torrent { bytes: bytes::Bytes, name: String, sizes: Vec<u64> },
}

#[derive(Serialize, Clone)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Submitted {
    Added { ids: Vec<u64> },
    Duplicate { id: u64 },
    Video { token: String, info: ytdlp::Info },
    Stream { token: String, name: String, choices: Vec<hls::Choice> },
    Torrent { token: String, name: String, files: Vec<torrent::File> },
    Links { links: Vec<grab::Link>, headers: Vec<(String, String)> },
    Nothing { reason: String },
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Tick {
    id: u64,
    done: u64,
    size: u64,
    speed: f64,
    lanes: Vec<[f32; 3]>,
    waiting: bool,
    preparing: bool,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Clip {
    kind: &'static str,
    title: String,
    detail: String,
    host: String,
}

pub struct App {
    pub handle: AppHandle,
    saved: Mutex<Saved>,
    live: Mutex<HashMap<u64, Live>>,
    pending: Mutex<HashMap<String, Pending>>,
    pub net: Arc<Net>,
    torrents: torrent::Torrents,
    data: PathBuf,
    dirty: AtomicBool,
    counter: AtomicU64,
    clip: Mutex<Option<(String, Clip, bool)>>,
    ignore: Mutex<Option<String>>,
    quitting: AtomicBool,
}

struct ItemHooks {
    app: Arc<App>,
    id: u64,
}

impl Hooks for ItemHooks {
    fn named(&self, name: &str, path: &Path, size: Option<u64>) {
        self.app.edit(self.id, |it| {
            it.name = name.to_string();
            it.path = Some(path.to_path_buf());
            if size.is_some() {
                it.size = size;
            }
        });
    }

    fn save(&self, resume: Resume) {
        self.app.quiet(self.id, |it| it.resume = Some(resume));
    }
}

fn err(f: Fail) -> String {
    f.message()
}

impl App {
    pub fn new(handle: AppHandle, data: PathBuf) -> Arc<Self> {
        let _ = std::fs::create_dir_all(&data);
        let mut saved = store::load(&data.join("state.json"));
        for it in saved.items.iter_mut() {
            if it.status == Status::Running && it.kind != Kind::Torrent {
                it.status = Status::Queued;
            }
        }
        let net = Arc::new(Net {
            client: engine::client(),
            limiter: engine::Limiter::new(saved.settings.speed_cap),
            hosts: engine::hosts::Hosts::load(data.join("hosts.json")),
        });
        let torrents = torrent::Torrents::new(data.join("torrents"), saved.settings.dir.clone());
        Arc::new(Self {
            handle,
            saved: Mutex::new(saved),
            live: Mutex::new(HashMap::new()),
            pending: Mutex::new(HashMap::new()),
            net,
            torrents,
            data,
            dirty: AtomicBool::new(false),
            counter: AtomicU64::new(1),
            clip: Mutex::new(None),
            ignore: Mutex::new(None),
            quitting: AtomicBool::new(false),
        })
    }

    pub fn tools_dir(&self) -> PathBuf {
        self.data.join("tools")
    }

    pub fn settings(&self) -> Settings {
        self.saved.lock().settings.clone()
    }

    pub fn views(&self) -> Vec<View> {
        self.saved.lock().items.iter().map(Item::view).collect()
    }

    fn item(&self, id: u64) -> Option<Item> {
        self.saved.lock().items.iter().find(|i| i.id == id).cloned()
    }

    fn edit(&self, id: u64, f: impl FnOnce(&mut Item)) {
        let view = {
            let mut s = self.saved.lock();
            let Some(it) = s.items.iter_mut().find(|i| i.id == id) else { return };
            f(it);
            it.view()
        };
        self.dirty.store(true, Release);
        let _ = self.handle.emit("item", view);
    }

    fn quiet(&self, id: u64, f: impl FnOnce(&mut Item)) {
        let mut s = self.saved.lock();
        if let Some(it) = s.items.iter_mut().find(|i| i.id == id) {
            f(it);
            self.dirty.store(true, Release);
        }
    }

    pub fn persist(&self) {
        if self.dirty.swap(false, AcqRel) {
            let s = self.saved.lock();
            let _ = store::save(&self.data.join("state.json"), &s);
        }
    }

    fn token(&self) -> String {
        format!("{:x}{:x}", store::now(), self.counter.fetch_add(1, Relaxed))
    }

    fn job(&self, token: CancellationToken, progress: Arc<Progress>) -> Job {
        Job { net: self.net.clone(), token, progress, conns: self.saved.lock().settings.connections }
    }

    fn scratch(&self) -> Job {
        self.job(CancellationToken::new(), Arc::new(Progress::default()))
    }

    fn push(self: &Arc<Self>, mut it: Item) -> u64 {
        let view = {
            let mut s = self.saved.lock();
            s.next_id += 1;
            it.id = s.next_id;
            let v = it.view();
            s.items.push(it);
            v
        };
        self.dirty.store(true, Release);
        let _ = self.handle.emit("item", view.clone());
        self.pump();
        view.id
    }

    fn blank(&self, kind: Kind, name: String, urls: Vec<String>, headers: Vec<(String, String)>) -> Item {
        Item {
            id: 0,
            kind,
            status: Status::Queued,
            name,
            dir: self.saved.lock().settings.dir.clone(),
            path: None,
            urls,
            headers,
            size: None,
            done: 0,
            created: store::now(),
            finished: None,
            error: None,
            resume: None,
            media: None,
            torrent: None,
            uploaded: 0,
        }
    }

    pub fn pump(self: &Arc<Self>) {
        if self.quitting.load(Acquire) {
            return;
        }
        let start: Vec<u64> = {
            let s = self.saved.lock();
            let live = self.live.lock();
            let limit = s.settings.at_once.max(1) as usize;
            let running = s.items.iter().filter(|i| i.kind != Kind::Torrent && live.contains_key(&i.id)).count();
            s.items
                .iter()
                .filter(|i| i.status == Status::Queued && i.kind != Kind::Torrent && !live.contains_key(&i.id))
                .take(limit.saturating_sub(running))
                .map(|i| i.id)
                .collect()
        };
        for id in start {
            self.start(id);
        }
    }

    fn start(self: &Arc<Self>, id: u64) {
        let token = CancellationToken::new();
        let progress = Arc::new(Progress::default());
        if let Some(it) = self.item(id) {
            progress.done.store(it.done, Relaxed);
            progress.size.store(it.size.unwrap_or(0), Relaxed);
        }
        self.live.lock().insert(id, Live { token: token.clone(), progress: progress.clone(), last: (Instant::now(), 0), speed: 0.0 });
        self.edit(id, |it| {
            it.status = Status::Running;
            it.error = None;
        });
        let app = self.clone();
        tauri::async_runtime::spawn(async move {
            let job = app.job(token, progress);
            let r = app.run(id, &job).await;
            app.live.lock().remove(&id);
            app.finish(id, r, &job);
            app.pump();
        });
    }

    fn finish(self: &Arc<Self>, id: u64, r: Result<engine::Done, Fail>, job: &Job) {
        let done = job.progress.done.load(Relaxed);
        match r {
            Ok(d) => {
                let name = d.path.file_name().map(|n| n.to_string_lossy().into_owned());
                self.edit(id, |it| {
                    it.status = Status::Done;
                    it.size = Some(d.size);
                    it.done = d.size;
                    if let Some(n) = &name {
                        it.name = n.clone();
                    }
                    it.path = Some(d.path.clone());
                    it.finished = Some(store::now());
                    it.resume = None;
                    it.error = None;
                });
                self.notify(id);
            }
            Err(Fail::Stopped) => self.edit(id, |it| {
                it.done = done;
                if it.status == Status::Running {
                    it.status = if self.quitting.load(Acquire) { Status::Queued } else { Status::Paused };
                }
            }),
            Err(e @ Fail::Expired(_)) => self.edit(id, |it| {
                it.status = Status::Expired;
                it.done = done;
                it.error = Some(e.message());
            }),
            Err(e) => self.edit(id, |it| {
                it.status = Status::Failed;
                it.done = done;
                it.error = Some(e.message());
            }),
        }
    }

    fn notify(&self, id: u64) {
        let focused = self.handle.get_webview_window("main").and_then(|w| w.is_focused().ok()).unwrap_or(false);
        if focused {
            return;
        }
        if let Some(it) = self.item(id) {
            use tauri_plugin_notification::NotificationExt;
            let folder = it.dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let _ = self.handle.notification().builder().title(&it.name).body(format!("Saved to {folder}")).show();
        }
    }

    async fn run(self: &Arc<Self>, id: u64, job: &Job) -> Result<engine::Done, Fail> {
        let it = self.item(id).ok_or(Fail::Stopped)?;
        let hooks = ItemHooks { app: self.clone(), id };
        match it.kind {
            Kind::File => {
                let t = Target {
                    sources: it.urls.clone(),
                    headers: it.headers.clone(),
                    dir: it.dir.clone(),
                    name: None,
                    path: it.path.clone(),
                    resume: it.resume.clone(),
                    allow_page: false,
                    mark: true,
                };
                engine::fetch(job, &t, &hooks).await
            }
            Kind::Ftp => {
                let t = Target {
                    sources: it.urls.clone(),
                    headers: Vec::new(),
                    dir: it.dir.clone(),
                    name: None,
                    path: it.path.clone(),
                    resume: it.resume.clone(),
                    allow_page: true,
                    mark: true,
                };
                engine::ftp::fetch(job, &t, &hooks).await
            }
            Kind::Stream => self.run_stream(&it, job, &hooks).await,
            Kind::Video => self.run_video(&it, job, &hooks).await,
            Kind::Torrent => Err(Fail::Stopped),
        }
    }

    fn work_dir(&self, it: &Item) -> PathBuf {
        it.dir.join(format!(".ddd-{}", it.id))
    }

    fn final_path(&self, it: &Item, ext: &str, hooks: &ItemHooks) -> PathBuf {
        if let Some(p) = &it.path {
            return p.clone();
        }
        let (stem, _) = filename::split_ext(&it.name);
        let p = filename::unique(&it.dir, &filename::sanitize(&format!("{stem}.{ext}")), &[]);
        hooks.named(&p.file_name().unwrap_or_default().to_string_lossy(), &p, None);
        p
    }

    async fn pull_tracks(&self, it: &Item, job: &Job, tracks: &[&media::Track], work: &Path) -> Result<Vec<PathBuf>, Fail> {
        let headers = probe::header_map(&it.headers);
        let tally = Tally::default();
        tally.total.store(tracks.iter().map(|t| t.pieces.len().max(1) as u64).sum(), Relaxed);
        let marks = it.resume.as_ref().map(|r| r.holes.clone()).unwrap_or_default();
        let mut outs = Vec::new();
        for (i, t) in tracks.iter().enumerate() {
            let out = work.join(format!("t{i}.{}", t.ext));
            let start = marks.get(i).map(|&(d, b)| (d as usize, b)).unwrap_or((0, 0));
            let id = it.id;
            let app = self;
            let save = move |d: usize, b: u64| {
                app.quiet(id, |x| {
                    let r = x.resume.get_or_insert_with(Resume::default);
                    if r.holes.len() <= i {
                        r.holes.resize(i + 1, (0, 0));
                    }
                    r.holes[i] = (d as u64, b);
                })
            };
            media::pull(job, &headers, t, &out, start, &tally, &save).await?;
            outs.push(out);
        }
        Ok(outs)
    }

    async fn assemble(&self, job: &Job, parts: Vec<PathBuf>, path: &Path, work: &Path) -> Result<engine::Done, Fail> {
        let out = if parts.len() == 1 && parts[0].extension() == path.extension() {
            std::fs::rename(&parts[0], path).map_err(|e| Fail::Disk(e.to_string()))?;
            path.to_path_buf()
        } else {
            let ff = tools::ffmpeg(job, &self.tools_dir()).await;
            match ff {
                Ok(ff) => media::mux(job, &ff, &parts, path).await?,
                Err(Fail::Stopped) => return Err(Fail::Stopped),
                Err(_) if parts.len() == 1 => {
                    let p = path.with_extension(parts[0].extension().unwrap_or_default());
                    std::fs::rename(&parts[0], &p).map_err(|e| Fail::Disk(e.to_string()))?;
                    p
                }
                Err(e) => return Err(e),
            }
        };
        let _ = std::fs::remove_dir_all(work);
        let size = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
        Ok(engine::Done { path: out, size })
    }

    async fn run_stream(&self, it: &Item, job: &Job, hooks: &ItemHooks) -> Result<engine::Done, Fail> {
        let url = it.urls.first().cloned().unwrap_or_default();
        let headers = probe::header_map(&it.headers);
        let work = self.work_dir(it);
        std::fs::create_dir_all(&work).map_err(|e| Fail::Disk(e.to_string()))?;
        let (video, audio) = match &it.media {
            Some(Media::Dash { height }) => {
                let p = dash::plan(job, &url, &headers, *height).await?;
                (p.video.or(p.audio.clone()).ok_or_else(|| Fail::Other("That stream has nothing to save".into()))?, p.audio)
            }
            Some(Media::Hls { variant }) => {
                let p = hls::plan(job, &url, &headers, *variant).await?;
                (p.video, p.audio)
            }
            _ => {
                let p = hls::plan(job, &url, &headers, None).await?;
                (p.video, p.audio)
            }
        };
        let mut tracks = vec![&video];
        if let Some(a) = audio.as_ref().filter(|a| a.pieces.len() != video.pieces.len() || a.whole != video.whole || a.init.as_ref().map(|i| &i.url) != video.init.as_ref().map(|i| &i.url)) {
            tracks.push(a);
        }
        let path = self.final_path(it, "mp4", hooks);
        let parts = self.pull_tracks(it, job, &tracks, &work).await?;
        self.assemble(job, parts, &path, &work).await
    }

    async fn run_video(&self, it: &Item, job: &Job, hooks: &ItemHooks) -> Result<engine::Done, Fail> {
        let url = it.urls.first().cloned().unwrap_or_default();
        let exe = tools::ytdlp(job, &self.tools_dir()).await?;
        let (height, audio_only) = match &it.media {
            Some(Media::Ytdlp { height, audio }) => (*height, *audio),
            _ => (None, false),
        };
        job.progress.preparing.store(true, Relaxed);
        let json = ytdlp::probe(&exe, &url, &it.headers, &job.token).await;
        job.progress.preparing.store(false, Relaxed);
        let json = json?;
        let pick = ytdlp::pick(&json, height, audio_only);
        let path = self.final_path(it, &pick.ext, hooks);
        let fmts: Vec<&ytdlp::Fmt> = [pick.video.as_ref(), pick.audio.as_ref()].into_iter().flatten().collect();
        if fmts.is_empty() {
            return Err(Fail::Other("No downloadable format was found".into()));
        }
        let work = self.work_dir(it);
        std::fs::create_dir_all(&work).map_err(|e| Fail::Disk(e.to_string()))?;
        let native = fmts.iter().all(|f| ytdlp::native_ok(f));
        if native {
            match self.video_native(it, job, &fmts, &work).await {
                Ok(parts) => return self.assemble(job, parts, &path, &work).await,
                Err(Fail::Stopped) => return Err(Fail::Stopped),
                Err(_) => {}
            }
        }
        let ff = tools::ffmpeg(job, &self.tools_dir()).await.ok();
        let spec = fmts.iter().map(|f| f.id.as_str()).collect::<Vec<_>>().join("+");
        let tmp = work.join(path.file_name().unwrap_or_default());
        let got = ytdlp::run(job, &exe, ff.as_deref(), &url, &spec, &tmp, &it.headers).await?;
        let dest = path.with_extension(got.extension().unwrap_or_default());
        std::fs::rename(&got, &dest).map_err(|e| Fail::Disk(e.to_string()))?;
        let _ = std::fs::remove_dir_all(&work);
        let size = std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0);
        Ok(engine::Done { path: dest, size })
    }

    async fn video_native(&self, it: &Item, job: &Job, fmts: &[&ytdlp::Fmt], work: &Path) -> Result<Vec<PathBuf>, Fail> {
        let mut parts = Vec::new();
        let mut sizes = 0u64;
        for (i, f) in fmts.iter().enumerate() {
            if f.protocol.starts_with("m3u8") {
                let headers = probe::header_map(&f.headers);
                let p = hls::plan(job, &f.url, &headers, None).await?;
                let tracks = vec![&p.video];
                let sub = Item { headers: f.headers.clone(), resume: None, ..it.clone() };
                let mut got = self.pull_tracks(&sub, job, &tracks, work).await?;
                let renamed = work.join(format!("f{i}.{}", p.video.ext));
                std::fs::rename(got.remove(0), &renamed).map_err(|e| Fail::Disk(e.to_string()))?;
                parts.push(renamed);
            } else {
                let out = work.join(format!("f{i}.{}", f.ext));
                let sub = Job { net: job.net.clone(), token: job.token.clone(), progress: Arc::new(Progress::default()), conns: job.conns };
                let n = {
                    let fut = engine::to_file(&sub, &f.url, &f.headers, &out);
                    tokio::pin!(fut);
                    let mut tick = tokio::time::interval(Duration::from_millis(250));
                    loop {
                        tokio::select! {
                            r = &mut fut => break r?,
                            _ = tick.tick() => {
                                job.progress.done.store(sizes + sub.progress.done.load(Relaxed), Relaxed);
                                let s = sub.progress.size.load(Relaxed);
                                if s > 0 {
                                    job.progress.size.store((sizes + s).max(job.progress.size.load(Relaxed)), Relaxed);
                                }
                                *job.progress.lanes.lock() = sub.progress.lanes.lock().clone();
                            }
                        }
                    }
                };
                sizes += n;
                parts.push(out);
            }
        }
        Ok(parts)
    }

    pub async fn submit(self: &Arc<Self>, text: String) -> Result<Submitted, String> {
        let t = text.trim().to_string();
        if t.is_empty() {
            return Ok(Submitted::Nothing { reason: String::new() });
        }
        if let Some(req) = curl::parse(&t) {
            return self.submit_link(req.url, req.headers).await;
        }
        if t.starts_with("magnet:") && !t.contains(char::is_whitespace) {
            return self.submit_torrent(&t).await;
        }
        let p = Path::new(&t);
        if p.is_file() && p.extension().is_some_and(|e| e.eq_ignore_ascii_case("torrent")) {
            return self.submit_torrent(&t).await;
        }
        if t.contains(char::is_whitespace) {
            let urls = grab::urls_in(&t);
            if urls.len() == 1 {
                return self.submit_link(urls[0].clone(), Vec::new()).await;
            }
            let mut links = grab::extract(&t, None);
            if links.is_empty() {
                links = urls
                    .iter()
                    .map(|u| grab::Link { url: u.clone(), name: filename::from_url(u).unwrap_or_else(|| u.clone()), group: grab::Group::Other })
                    .collect();
            }
            if links.is_empty() {
                return Ok(Submitted::Nothing { reason: "No links in that text".into() });
            }
            return Ok(Submitted::Links { links, headers: Vec::new() });
        }
        let url = if t.contains("://") { t } else { format!("https://{t}") };
        if url::Url::parse(&url).is_err() {
            return Ok(Submitted::Nothing { reason: "That doesn't look like a link".into() });
        }
        self.submit_link(url, Vec::new()).await
    }

    fn duplicate(&self, url: &str) -> Option<u64> {
        let s = self.saved.lock();
        s.items
            .iter()
            .rev()
            .find(|i| {
                i.urls.first().map(String::as_str) == Some(url)
                    && match i.status {
                        Status::Done => i.path.as_ref().is_some_and(|p| p.exists()),
                        Status::Queued | Status::Running | Status::Paused => true,
                        _ => false,
                    }
            })
            .map(|i| i.id)
    }

    async fn submit_link(self: &Arc<Self>, url: String, headers: Vec<(String, String)>) -> Result<Submitted, String> {
        if let Some(id) = self.duplicate(&url) {
            return Ok(Submitted::Duplicate { id });
        }
        let lower = url.to_ascii_lowercase();
        let path_lower = url::Url::parse(&lower).map(|u| u.path().to_string()).unwrap_or_default();
        if lower.starts_with("ftp://") {
            let name = filename::from_url(&url).unwrap_or_else(|| "download".into());
            let id = self.push(self.blank(Kind::Ftp, name, vec![url], Vec::new()));
            return Ok(Submitted::Added { ids: vec![id] });
        }
        if path_lower.ends_with(".m3u8") || path_lower.ends_with(".mpd") {
            return self.submit_stream(url, headers).await;
        }
        if path_lower.ends_with(".torrent") {
            let job = self.scratch();
            let tmp = self.data.join(format!("dl-{}.torrent", self.token()));
            engine::to_file(&job, &url, &headers, &tmp).await.map_err(err)?;
            let r = self.submit_torrent(&tmp.to_string_lossy()).await;
            let _ = std::fs::remove_file(&tmp);
            return r;
        }
        if clipboard::video_host(&url) {
            return self.submit_video(url, headers).await;
        }
        let job = self.scratch();
        let h = probe::header_map(&headers);
        let head = engine::retry(&job.token, || probe::head(&self.net.client, &url, &h)).await;
        match head {
            Ok(head) if !head.is_page() => {
                let mut it = self.blank(Kind::File, head.file_name(&url), vec![url], headers);
                it.size = head.size;
                let id = self.push(it);
                Ok(Submitted::Added { ids: vec![id] })
            }
            Ok(head) => self.submit_page(head.url, headers).await,
            Err(Fail::Expired(c)) if c != 404 => {
                if tools::ytdlp_ready(&self.tools_dir()).is_some() {
                    if let Ok(s) = self.submit_video(url.clone(), headers.clone()).await {
                        return Ok(s);
                    }
                }
                Err(Fail::Expired(c).message())
            }
            Err(e) => Err(e.message()),
        }
    }

    async fn submit_page(self: &Arc<Self>, url: String, headers: Vec<(String, String)>) -> Result<Submitted, String> {
        let job = self.scratch();
        let h = probe::header_map(&headers);
        let html = media::text(&job, &url, &h).await.map(|(_, b)| b).unwrap_or_default();
        let mut links = grab::extract(&html, Some(&url));
        let lower = html.to_ascii_lowercase();
        let has_player = ["<video", ".m3u8", ".mpd", "youtube.com/embed", "player.vimeo.com", "og:video", "jwplayer", "videojs"]
            .iter()
            .any(|k| lower.contains(k));
        if has_player || links.is_empty() {
            if let Ok(s @ Submitted::Video { .. }) = self.submit_video(url.clone(), headers.clone()).await {
                return Ok(s);
            }
        }
        links.retain(|l| l.url != url);
        if links.is_empty() {
            return Ok(Submitted::Nothing { reason: "Nothing to download on that page".into() });
        }
        Ok(Submitted::Links { links, headers })
    }

    async fn submit_video(self: &Arc<Self>, url: String, headers: Vec<(String, String)>) -> Result<Submitted, String> {
        let job = self.scratch();
        let exe = tools::ytdlp(&job, &self.tools_dir()).await.map_err(err)?;
        let json: Value = ytdlp::probe(&exe, &url, &headers, &job.token).await.map_err(err)?;
        let info = ytdlp::info(&json);
        let token = self.token();
        self.pending.lock().insert(token.clone(), Pending::Video { url, headers, title: info.title.clone() });
        Ok(Submitted::Video { token, info })
    }

    async fn submit_stream(self: &Arc<Self>, url: String, headers: Vec<(String, String)>) -> Result<Submitted, String> {
        let job = self.scratch();
        let h = probe::header_map(&headers);
        let is_dash = url::Url::parse(&url).map(|u| u.path().to_ascii_lowercase().ends_with(".mpd")).unwrap_or(false);
        let name = filename::from_url(&url).map(|n| filename::split_ext(&n).0.to_string()).filter(|n| n != "master" && n != "index" && n != "playlist" && n != "manifest").unwrap_or_else(|| {
            url::Url::parse(&url).ok().and_then(|u| u.host_str().map(|h| h.trim_start_matches("www.").to_string())).unwrap_or_else(|| "stream".into())
        });
        let choices = if is_dash {
            let p = dash::plan(&job, &url, &h, None).await.map_err(err)?;
            p.heights.iter().map(|&h| hls::Choice { id: h as u64, height: Some(h), label: format!("{h}p") }).collect()
        } else {
            hls::choices(&job, &url, &h).await.map_err(err)?
        };
        if choices.len() <= 1 {
            let mut it = self.blank(Kind::Stream, format!("{name}.mp4"), vec![url], headers);
            it.media = Some(if is_dash { Media::Dash { height: None } } else { Media::Hls { variant: None } });
            let id = self.push(it);
            return Ok(Submitted::Added { ids: vec![id] });
        }
        let token = self.token();
        self.pending.lock().insert(token.clone(), Pending::Stream { url, headers, dash: is_dash, name: name.clone() });
        Ok(Submitted::Stream { token, name, choices })
    }

    async fn submit_torrent(self: &Arc<Self>, src: &str) -> Result<Submitted, String> {
        let meta = self.torrents.inspect(src).await.map_err(err)?;
        if let Some(id) = self.saved.lock().items.iter().find(|i| i.torrent.as_ref().is_some_and(|t| t.hash == meta.hash)).map(|i| i.id) {
            return Ok(Submitted::Duplicate { id });
        }
        let token = self.token();
        let sizes = meta.files.iter().map(|f| f.size).collect();
        self.pending.lock().insert(token.clone(), Pending::Torrent { bytes: meta.bytes.clone(), name: meta.name.clone(), sizes });
        Ok(Submitted::Torrent { token, name: meta.name, files: meta.files })
    }

    pub fn add_links(self: &Arc<Self>, urls: Vec<String>, headers: Vec<(String, String)>) -> Vec<u64> {
        let mut ids = Vec::new();
        for url in urls {
            if self.duplicate(&url).is_some() {
                continue;
            }
            let lower = url.to_ascii_lowercase();
            if lower.starts_with("magnet:") || lower.ends_with(".torrent") || lower.contains(".m3u8") || lower.contains(".mpd") || clipboard::video_host(&url) {
                let app = self.clone();
                let h = headers.clone();
                tauri::async_runtime::spawn(async move {
                    match app.submit_link_or_magnet(url, h).await {
                        Ok(Submitted::Torrent { token, .. }) => {
                            let _ = app.add_torrent(token, None).await;
                        }
                        Ok(Submitted::Stream { token, .. }) => {
                            let _ = app.add_stream(token, None);
                        }
                        Ok(Submitted::Video { token, .. }) => {
                            let _ = app.add_video(token, None, false);
                        }
                        _ => {}
                    }
                });
                continue;
            }
            let kind = if lower.starts_with("ftp://") { Kind::Ftp } else { Kind::File };
            let name = filename::from_url(&url).map(|n| filename::sanitize(&n)).unwrap_or_else(|| "download".into());
            ids.push(self.push(self.blank(kind, name, vec![url], headers.clone())));
        }
        ids
    }

    async fn submit_link_or_magnet(self: &Arc<Self>, url: String, headers: Vec<(String, String)>) -> Result<Submitted, String> {
        if url.starts_with("magnet:") {
            self.submit_torrent(&url).await
        } else {
            self.submit_link(url, headers).await
        }
    }

    pub fn add_video(self: &Arc<Self>, token: String, height: Option<u32>, audio: bool) -> Result<u64, String> {
        let Some(Pending::Video { url, headers, title }) = self.pending.lock().remove(&token) else {
            return Err("That choice expired. Paste the link again.".into());
        };
        let mut it = self.blank(Kind::Video, filename::sanitize(&if title.is_empty() { "video".into() } else { title }), vec![url], headers);
        it.media = Some(Media::Ytdlp { height, audio });
        Ok(self.push(it))
    }

    pub fn add_stream(self: &Arc<Self>, token: String, choice: Option<u64>) -> Result<u64, String> {
        let Some(Pending::Stream { url, headers, dash, name }) = self.pending.lock().remove(&token) else {
            return Err("That choice expired. Paste the link again.".into());
        };
        let mut it = self.blank(Kind::Stream, format!("{name}.mp4"), vec![url], headers);
        it.media = Some(if dash { Media::Dash { height: choice.map(|c| c as u32) } } else { Media::Hls { variant: choice } });
        Ok(self.push(it))
    }

    pub async fn add_torrent(self: &Arc<Self>, token: String, files: Option<Vec<usize>>) -> Result<u64, String> {
        let Some(Pending::Torrent { bytes, name, sizes }) = self.pending.lock().remove(&token) else {
            return Err("That choice expired. Paste the link again.".into());
        };
        let dir = self.settings().dir;
        let hash = self.torrents.add(&bytes, &dir, files.clone()).await.map_err(err)?;
        let saved = self.data.join("torrents").join(format!("{hash}.torrent"));
        let _ = std::fs::create_dir_all(saved.parent().unwrap_or(&self.data));
        let _ = std::fs::write(&saved, &bytes);
        let size = match &files {
            Some(f) => f.iter().filter_map(|i| sizes.get(*i)).sum(),
            None => sizes.iter().sum(),
        };
        let mut it = self.blank(Kind::Torrent, filename::sanitize(&name), Vec::new(), Vec::new());
        it.status = Status::Running;
        it.size = Some(size);
        it.path = Some(dir.join(filename::sanitize(&name)));
        it.torrent = Some(Torrent { hash, files, source: saved.to_string_lossy().into_owned() });
        Ok(self.push(it))
    }

    pub async fn pause(self: &Arc<Self>, id: u64) {
        let Some(it) = self.item(id) else { return };
        if let Some(t) = &it.torrent {
            let _ = self.torrents.pause(&t.hash).await;
            self.edit(id, |i| i.status = Status::Paused);
            return;
        }
        if matches!(it.status, Status::Running | Status::Queued) {
            self.edit(id, |i| i.status = Status::Paused);
        }
        if let Some(l) = self.live.lock().get(&id) {
            l.token.cancel();
        }
    }

    pub async fn resume(self: &Arc<Self>, id: u64) {
        let Some(it) = self.item(id) else { return };
        if let Some(t) = &it.torrent {
            if !self.torrents.has(&t.hash) {
                if let Ok(bytes) = std::fs::read(&t.source) {
                    let _ = self.torrents.add(&bytes, &it.dir, t.files.clone()).await;
                }
            }
            let _ = self.torrents.resume(&t.hash).await;
            self.edit(id, |i| {
                i.status = Status::Running;
                i.error = None;
            });
            return;
        }
        if matches!(it.status, Status::Paused | Status::Failed | Status::Expired) {
            self.edit(id, |i| {
                i.status = Status::Queued;
                i.error = None;
            });
            self.pump();
        }
    }

    pub async fn remove(self: &Arc<Self>, id: u64, delete: bool) {
        let Some(it) = self.item(id) else { return };
        if let Some(l) = self.live.lock().remove(&id) {
            l.token.cancel();
        }
        self.saved.lock().items.retain(|i| i.id != id);
        self.dirty.store(true, Release);
        let _ = self.handle.emit("removed", id);
        if let Some(t) = &it.torrent {
            let _ = self.torrents.remove(&t.hash, delete).await;
            let _ = std::fs::remove_file(&t.source);
        } else {
            let work = self.work_dir(&it);
            tokio::time::sleep(Duration::from_millis(300)).await;
            let _ = std::fs::remove_dir_all(work);
            if let Some(p) = &it.path {
                let _ = std::fs::remove_file(filename::part_path(p));
                if delete && it.status == Status::Done {
                    let _ = std::fs::remove_file(p);
                }
            }
        }
        self.pump();
    }

    pub fn relink(self: &Arc<Self>, id: u64, text: String) -> Result<(), String> {
        let (url, headers) = match curl::parse(&text) {
            Some(r) => (r.url, Some(r.headers)),
            None => (grab::urls_in(&text).into_iter().next().unwrap_or_else(|| text.trim().to_string()), None),
        };
        if url::Url::parse(&url).is_err() {
            return Err("That doesn't look like a link".into());
        }
        self.edit(id, |it| {
            if let Some(first) = it.urls.first_mut() {
                *first = url;
            } else {
                it.urls.push(url);
            }
            if let Some(h) = headers {
                it.headers = h;
            }
            if matches!(it.status, Status::Expired | Status::Failed | Status::Paused) {
                it.status = Status::Queued;
                it.error = None;
            }
        });
        self.pump();
        Ok(())
    }

    pub fn path_of(&self, id: u64) -> Option<PathBuf> {
        let it = self.item(id)?;
        it.path.clone().filter(|p| p.exists()).or_else(|| it.path.as_ref().map(|p| filename::part_path(p)).filter(|p| p.exists())).or(Some(it.dir))
    }

    pub fn set_settings(self: &Arc<Self>, mut next: Settings) -> Settings {
        next.connections = next.connections.clamp(1, 32);
        next.at_once = next.at_once.clamp(1, 10);
        next.seed_ratio = next.seed_ratio.clamp(0.0, 10.0);
        if !next.dir.is_dir() {
            next.dir = store::downloads_dir();
        }
        let magnets_changed = self.saved.lock().settings.magnets != next.magnets;
        self.net.limiter.set(next.speed_cap);
        self.saved.lock().settings = next.clone();
        self.dirty.store(true, Release);
        if magnets_changed {
            crate::assoc::set(next.magnets);
        }
        self.pump();
        next
    }

    pub fn tick(self: &Arc<Self>) {
        let now = Instant::now();
        let mut out = Vec::new();
        {
            let mut live = self.live.lock();
            for (&id, l) in live.iter_mut() {
                let done = l.progress.done.load(Relaxed);
                let dt = now.duration_since(l.last.0).as_secs_f64().max(0.001);
                let inst = done.saturating_sub(l.last.1) as f64 / dt;
                l.speed = if l.last.1 == 0 && l.speed == 0.0 { inst } else { l.speed * 0.75 + inst * 0.25 };
                l.last = (now, done);
                out.push(Tick {
                    id,
                    done,
                    size: l.progress.size.load(Relaxed),
                    speed: l.speed,
                    lanes: l.progress.lanes.lock().clone(),
                    waiting: l.progress.waiting.load(Relaxed),
                    preparing: l.progress.preparing.load(Relaxed),
                });
            }
        }
        let mut finished_torrents = Vec::new();
        {
            let mut s = self.saved.lock();
            let ratio = s.settings.seed_ratio;
            for it in s.items.iter_mut() {
                if let Some(t) = out.iter().find(|t| t.id == it.id) {
                    it.done = t.done;
                    if t.size > 0 && it.kind == Kind::File {
                        it.size = Some(t.size);
                    }
                    continue;
                }
                let Some(tor) = &it.torrent else { continue };
                if !it.active() {
                    continue;
                }
                let Some(st) = self.torrents.stats(&tor.hash) else { continue };
                it.done = st.done;
                it.size = Some(st.total);
                it.uploaded = st.uploaded;
                out.push(Tick { id: it.id, done: st.done, size: st.total, speed: st.down, lanes: Vec::new(), waiting: false, preparing: st.total == 0 });
                if let Some(e) = st.error {
                    it.status = Status::Failed;
                    it.error = Some(e);
                    finished_torrents.push((it.id, false, tor.hash.clone()));
                } else if st.finished && it.status == Status::Running {
                    it.status = if ratio <= 0.0 { Status::Done } else { Status::Seeding };
                    it.finished = Some(store::now());
                    finished_torrents.push((it.id, ratio <= 0.0, tor.hash.clone()));
                } else if it.status == Status::Seeding && st.total > 0 && st.uploaded as f64 >= st.total as f64 * ratio {
                    it.status = Status::Done;
                    finished_torrents.push((it.id, true, tor.hash.clone()));
                }
            }
        }
        for (id, stop, hash) in finished_torrents {
            self.dirty.store(true, Release);
            if let Some(it) = self.item(id) {
                let _ = self.handle.emit("item", it.view());
            }
            if stop {
                let app = self.clone();
                tauri::async_runtime::spawn(async move {
                    let _ = app.torrents.pause(&hash).await;
                });
            }
            self.notify(id);
        }
        let total: f64 = out.iter().map(|t| t.speed).sum();
        proc::awake(!out.is_empty());
        let _ = self.handle.emit("progress", serde_json::json!({ "items": out, "total": total }));
    }

    pub async fn boot_torrents(self: &Arc<Self>) {
        let active: Vec<Item> = self.saved.lock().items.iter().filter(|i| i.torrent.is_some() && i.active()).cloned().collect();
        if active.is_empty() {
            return;
        }
        if self.torrents.session().await.is_err() {
            return;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
        for it in active {
            let t = it.torrent.as_ref().expect("torrent");
            if !self.torrents.has(&t.hash) {
                if let Ok(bytes) = std::fs::read(&t.source) {
                    let _ = self.torrents.add(&bytes, &it.dir, t.files.clone()).await;
                }
            }
            let _ = self.torrents.resume(&t.hash).await;
        }
    }

    pub fn has_active(&self) -> bool {
        !self.live.lock().is_empty() || self.saved.lock().items.iter().any(|i| i.torrent.is_some() && i.active())
    }

    pub async fn quit(self: &Arc<Self>) {
        self.quitting.store(true, Release);
        let tokens: Vec<_> = self.live.lock().values().map(|l| l.token.clone()).collect();
        for t in tokens {
            t.cancel();
        }
        let deadline = Instant::now() + Duration::from_millis(2500);
        while !self.live.lock().is_empty() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        self.dirty.store(true, Release);
        self.persist();
    }

    pub fn clip_ignore(&self, text: String) {
        *self.ignore.lock() = Some(text);
    }

    pub async fn submit_bytes(self: &Arc<Self>, name: String, data: Vec<u8>) -> Result<Submitted, String> {
        if !name.to_ascii_lowercase().ends_with(".torrent") || data.len() > 20_000_000 {
            return Ok(Submitted::Nothing { reason: "That file isn't something to download".into() });
        }
        let tmp = self.data.join(format!("drop-{}.torrent", self.token()));
        std::fs::write(&tmp, &data).map_err(|e| e.to_string())?;
        let r = self.submit_torrent(&tmp.to_string_lossy()).await;
        let _ = std::fs::remove_file(&tmp);
        r
    }

    pub fn on_clip(self: &Arc<Self>, text: String) {
        if !self.saved.lock().settings.catch {
            return;
        }
        if self.ignore.lock().take().is_some_and(|t| t == text) {
            return;
        }
        let Some(c) = clipboard::classify(&text) else { return };
        let app = self.clone();
        tauri::async_runtime::spawn(async move {
            let clip = match &c {
                Catch::Magnet(m) => {
                    let name = url::Url::parse(m)
                        .ok()
                        .and_then(|u| u.query_pairs().find(|(k, _)| k == "dn").map(|(_, v)| v.into_owned()))
                        .unwrap_or_else(|| "Torrent".into());
                    Clip { kind: "torrent", title: name, detail: String::new(), host: String::new() }
                }
                Catch::Many(n) => Clip { kind: "many", title: format!("{n} links"), detail: String::new(), host: String::new() },
                Catch::Curl(cmd) => {
                    let Some(r) = curl::parse(cmd) else { return };
                    app.clip_for_url(&r.url, &r.headers).await.unwrap_or(Clip {
                        kind: "file",
                        title: filename::from_url(&r.url).unwrap_or_else(|| "download".into()),
                        detail: String::new(),
                        host: host_of(&r.url),
                    })
                }
                Catch::Link(u) => match app.clip_for_url(u, &[]).await {
                    Some(c) => c,
                    None => return,
                },
            };
            *app.clip.lock() = Some((text.clone(), clip.clone(), false));
            let focused = app.handle.get_webview_window("main").and_then(|w| w.is_focused().ok()).unwrap_or(false);
            if focused {
                let _ = app.handle.emit_to("main", "clip", clip);
            } else {
                crate::catch::show(&app.handle, &clip);
            }
        });
    }

    async fn clip_for_url(&self, url: &str, headers: &[(String, String)]) -> Option<Clip> {
        let host = host_of(url);
        if clipboard::video_host(url) {
            return Some(Clip { kind: "video", title: host.clone(), detail: String::new(), host });
        }
        let lower = url.to_ascii_lowercase();
        if lower.contains(".m3u8") || lower.contains(".mpd") {
            return Some(Clip { kind: "video", title: filename::from_url(url).unwrap_or_else(|| host.clone()), detail: String::new(), host });
        }
        if lower.starts_with("ftp://") {
            return Some(Clip { kind: "file", title: filename::from_url(url).unwrap_or_else(|| "download".into()), detail: String::new(), host });
        }
        let h = probe::header_map(headers);
        let head = tokio::time::timeout(Duration::from_secs(6), probe::head(&self.net.client, url, &h)).await.ok().and_then(|r| r.ok());
        match head {
            Some(head) if !head.is_page() => Some(Clip {
                kind: kind_of_name(&head.file_name(url)),
                title: head.file_name(url),
                detail: head.size.map(human).unwrap_or_default(),
                host,
            }),
            _ => None,
        }
    }

    pub fn clip_peek(&self) -> Option<Clip> {
        let mut c = self.clip.lock();
        match c.as_mut() {
            Some((_, clip, seen)) if !*seen => {
                *seen = true;
                Some(clip.clone())
            }
            _ => None,
        }
    }

    pub fn clip_text(&self) -> Option<String> {
        self.clip.lock().as_mut().map(|(t, _, seen)| {
            *seen = true;
            t.clone()
        })
    }

    pub async fn probe_sizes(&self, urls: Vec<String>, headers: Vec<(String, String)>) -> Vec<Option<u64>> {
        let h = probe::header_map(&headers);
        let futs = urls.into_iter().map(|u| {
            let h = h.clone();
            async move {
                if !u.starts_with("http") {
                    return None;
                }
                tokio::time::timeout(Duration::from_secs(10), probe::head(&self.net.client, &u, &h)).await.ok()?.ok()?.size
            }
        });
        futures_util::stream::iter(futs).buffered(8).collect().await
    }
}

use futures_util::StreamExt;

fn host_of(url: &str) -> String {
    url::Url::parse(url).ok().and_then(|u| u.host_str().map(|h| h.trim_start_matches("www.").to_string())).unwrap_or_default()
}

fn kind_of_name(name: &str) -> &'static str {
    match grab::group_of(&format!("https://x/{name}")) {
        Some(grab::Group::Video) => "video",
        Some(grab::Group::Audio) => "audio",
        Some(grab::Group::Archive) => "archive",
        Some(grab::Group::Document) => "document",
        Some(grab::Group::Image) => "image",
        Some(grab::Group::App) => "app",
        Some(grab::Group::Torrent) => "torrent",
        _ => "file",
    }
}

pub fn human(b: u64) -> String {
    let units = ["B", "KB", "MB", "GB", "TB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1000.0 && i < units.len() - 1 {
        v /= 1000.0;
        i += 1;
    }
    if i == 0 {
        format!("{b} B")
    } else if v >= 100.0 {
        format!("{v:.0} {}", units[i])
    } else {
        format!("{v:.1} {}", units[i])
    }
}
