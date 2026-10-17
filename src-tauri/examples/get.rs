use ddd_lib::engine::{self, Hooks, Job, Net, Progress, Target};
use ddd_lib::store::Resume;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;
use std::time::Instant;
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct Print {
    resume: parking_lot::Mutex<Option<Resume>>,
    path: parking_lot::Mutex<Option<PathBuf>>,
}
impl Hooks for Print {
    fn named(&self, name: &str, path: &Path, size: Option<u64>) {
        println!("name {name} size {size:?}");
        *self.path.lock() = Some(path.to_path_buf());
    }
    fn save(&self, r: Resume) {
        *self.resume.lock() = Some(r);
    }
}

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let url = args.next().expect("url");
    let conns: u32 = args.next().and_then(|c| c.parse().ok()).unwrap_or(16);
    let dir = std::env::temp_dir().join("ddd-bench");
    std::fs::create_dir_all(&dir).unwrap();
    let net = Arc::new(Net {
        client: engine::client(),
        limiter: engine::Limiter::new(0),
        hosts: engine::hosts::Hosts::load(dir.join("hosts.json")),
    });
    let progress = Arc::new(Progress::default());
    let job = Job { net, token: CancellationToken::new(), progress: progress.clone(), conns };
    let p = progress.clone();
    let ticker = tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            println!("{:>8.1} MB  lanes {}", p.done.load(Relaxed) as f64 / 1e6, p.lanes.lock().len());
        }
    });
    let mut t = Target {
        sources: vec![url],
        headers: vec![],
        dir: PathBuf::from(&dir),
        name: None,
        path: None,
        resume: None,
        allow_page: true,
        mark: false,
    };
    let start = Instant::now();
    let hooks = Print::default();
    let mut job = job;
    if std::env::var_os("DDD_RESUME_TEST").is_some() {
        let token = job.token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            token.cancel();
        });
        let r = engine::fetch(&job, &t, &hooks).await;
        let resume = hooks.resume.lock().clone().unwrap();
        let left: u64 = resume.holes.iter().map(|(a, b)| b - a).sum();
        println!("stopped: {:?}, {} holes, {:.1} MB left", r.err().map(|e| e.message()), resume.holes.len(), left as f64 / 1e6);
        t.path = hooks.path.lock().clone();
        t.resume = Some(resume);
        job = Job { net: job.net.clone(), token: CancellationToken::new(), progress: job.progress.clone(), conns: job.conns };
    }
    let r = engine::fetch(&job, &t, &hooks).await;
    ticker.abort();
    let secs = start.elapsed().as_secs_f64();
    match r {
        Ok(d) => {
            println!("done {} in {secs:.2}s = {:.1} MB/s", d.path.display(), d.size as f64 / 1e6 / secs);
            let bytes = std::fs::read(&d.path).unwrap();
            use sha2::Digest;
            println!("sha256 {:x}", sha2::Sha256::digest(&bytes));
            let _ = std::fs::remove_file(&d.path);
        }
        Err(e) => println!("failed: {}", e.message()),
    }
}
