use super::splitter::{Plan, Seg};
use super::{writer, Done, Fail, Hooks, Job, Target};
use crate::filename;
use crate::store::Resume;
use bytes::BytesMut;
use percent_encoding::percent_decode_str;
use std::path::Path;
use std::sync::atomic::Ordering::*;
use std::sync::Arc;
use std::time::Duration;
use suppaftp::tokio::AsyncFtpStream;
use suppaftp::types::FileType;
use tokio::io::AsyncReadExt;
use tokio::task::JoinSet;

#[derive(Clone)]
struct Spot {
    addr: String,
    user: String,
    pass: String,
    path: String,
}

fn spot(url: &str) -> Result<Spot, Fail> {
    let u = url::Url::parse(url).map_err(|_| Fail::Other("That FTP link doesn't look right".into()))?;
    let host = u.host_str().ok_or_else(|| Fail::Other("That FTP link has no server".into()))?;
    let user = match u.username() {
        "" => "anonymous".to_string(),
        n => percent_decode_str(n).decode_utf8_lossy().into_owned(),
    };
    let pass = u.password().map(|p| percent_decode_str(p).decode_utf8_lossy().into_owned()).unwrap_or_else(|| "anonymous@".into());
    Ok(Spot {
        addr: format!("{host}:{}", u.port().unwrap_or(21)),
        user,
        pass,
        path: percent_decode_str(u.path()).decode_utf8_lossy().into_owned(),
    })
}

fn ftp_fail(e: suppaftp::FtpError) -> Fail {
    match e {
        suppaftp::FtpError::UnexpectedResponse(r) if r.status.code() == 550 => Fail::Expired(404),
        suppaftp::FtpError::UnexpectedResponse(r) if r.status.code() == 530 => Fail::Expired(401),
        e => Fail::Net(e.to_string()),
    }
}

async fn connect(s: &Spot) -> Result<AsyncFtpStream, Fail> {
    let mut c = tokio::time::timeout(Duration::from_secs(20), AsyncFtpStream::connect(&s.addr))
        .await
        .map_err(|_| Fail::Net("timed out".into()))?
        .map_err(ftp_fail)?;
    c.login(s.user.as_str(), s.pass.as_str()).await.map_err(ftp_fail)?;
    c.transfer_type(FileType::Binary).await.map_err(ftp_fail)?;
    Ok(c)
}

pub async fn fetch(job: &Job, t: &Target, hooks: &dyn Hooks) -> Result<Done, Fail> {
    let s = spot(&t.sources[0])?;
    let mut c = super::retry(&job.token, || connect(&s)).await?;
    let size = c.size(&s.path).await.ok().map(|n| n as u64);
    let path = match &t.path {
        Some(p) => p.clone(),
        None => {
            let name = t.name.clone().or_else(|| filename::from_url(&t.sources[0])).unwrap_or_else(|| "download".into());
            let p = filename::unique(&t.dir, &filename::sanitize(&name), &[]);
            hooks.named(&p.file_name().unwrap_or_default().to_string_lossy(), &p, size);
            p
        }
    };
    let part = filename::part_path(&path);
    job.progress.size.store(size.unwrap_or(0), Relaxed);
    let total = match size {
        Some(size) if size > 0 => {
            let _ = c.quit().await;
            split(job, t, &s, &part, size, hooks).await?;
            size
        }
        _ => whole(job, c, &s, &part).await?,
    };
    if std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0) != total {
        return Err(Fail::Other("The download came out the wrong size".into()));
    }
    std::fs::rename(&part, &path).map_err(|e| Fail::Disk(e.to_string()))?;
    crate::motw::mark(&path, &t.sources[0], None);
    Ok(Done { path, size: total })
}

async fn whole(job: &Job, mut c: AsyncFtpStream, s: &Spot, part: &Path) -> Result<u64, Fail> {
    let mut stream = c.retr_as_stream(&s.path).await.map_err(ftp_fail)?;
    let mut f = tokio::fs::File::create(part).await.map_err(|e| Fail::Disk(e.to_string()))?;
    let mut buf = vec![0u8; 256 * 1024];
    let mut total = 0u64;
    loop {
        let n = tokio::select! {
            n = stream.read(&mut buf) => n.map_err(|e| Fail::Net(e.to_string()))?,
            _ = job.token.cancelled() => return Err(Fail::Stopped),
        };
        if n == 0 {
            break;
        }
        job.net.limiter.take(n).await;
        tokio::io::AsyncWriteExt::write_all(&mut f, &buf[..n]).await.map_err(|e| Fail::Disk(e.to_string()))?;
        total += n as u64;
        job.progress.done.store(total, Relaxed);
    }
    let _ = c.finalize_retr_stream(stream).await;
    let _ = c.quit().await;
    Ok(total)
}

async fn split(job: &Job, t: &Target, s: &Spot, part: &Path, size: u64, hooks: &dyn Hooks) -> Result<(), Fail> {
    let resumable = t
        .resume
        .as_ref()
        .filter(|r| r.size == size && std::fs::metadata(part).map(|m| m.len() == size).unwrap_or(false));
    let plan = Arc::new(match resumable {
        Some(r) => Plan::from_holes(size, &r.holes),
        None => Plan::new(size),
    });
    let w = writer::Writer::open(part, Some(size), resumable.is_none()).map_err(|e| Fail::Disk(e.to_string()))?;
    let mut set = JoinSet::new();
    for _ in 0..job.conns.clamp(1, 4) {
        let plan = plan.clone();
        let tx = w.tx();
        let s = s.clone();
        let token = job.token.clone();
        let net = job.net.clone();
        set.spawn(async move {
            while let Some(seg) = plan.claim() {
                let mut attempt = 0;
                loop {
                    match piece(&s, &seg, &tx, &token, &net).await {
                        Ok(()) if seg.left() == 0 => break,
                        Ok(()) => {}
                        Err(Fail::Stopped) => return Err(Fail::Stopped),
                        Err(e @ Fail::Expired(_)) => return Err(e),
                        Err(e) if attempt >= 8 => return Err(e),
                        Err(_) => {}
                    }
                    attempt += 1;
                    if super::sleep(&token, super::backoff(attempt)).await {
                        return Err(Fail::Stopped);
                    }
                }
                plan.release(&seg);
            }
            Ok(())
        });
    }
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    let mut fatal = None;
    let mut n = 0u64;
    while !set.is_empty() {
        tokio::select! {
            _ = tick.tick() => {
                n += 1;
                job.progress.done.store(plan.received(), Relaxed);
                *job.progress.lanes.lock() = plan.lanes();
                if n % 4 == 0 {
                    hooks.save(Resume { size, holes: plan.holes(), etag: None, modified: None });
                }
            }
            r = set.join_next() => {
                if let Some(Ok(Err(e))) = r {
                    if !matches!(e, Fail::Stopped) && fatal.is_none() {
                        fatal = Some(e);
                        job.token.cancel();
                    }
                }
            }
        }
    }
    w.finish().await.map_err(|e| Fail::Disk(e.to_string()))?;
    hooks.save(Resume { size, holes: plan.holes(), etag: None, modified: None });
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

async fn piece(
    s: &Spot,
    seg: &Arc<Seg>,
    tx: &writer::Tx,
    token: &tokio_util::sync::CancellationToken,
    net: &super::Net,
) -> Result<(), Fail> {
    let mut c = connect(s).await?;
    let start = seg.pos.load(Acquire);
    c.resume_transfer(start as usize).await.map_err(ftp_fail)?;
    let mut stream = c.retr_as_stream(&s.path).await.map_err(ftp_fail)?;
    let mut read = vec![0u8; 128 * 1024];
    let mut buf = BytesMut::with_capacity(512 * 1024);
    let mut off = start;
    let r = loop {
        let n = tokio::select! {
            n = stream.read(&mut read) => match n { Ok(n) => n, Err(e) => break Err(Fail::Net(e.to_string())) },
            _ = token.cancelled() => break Err(Fail::Stopped),
        };
        if n == 0 {
            break Ok(());
        }
        net.limiter.take(n).await;
        let pos = seg.pos.load(Acquire);
        let end = seg.end.load(Acquire);
        if pos >= end {
            break Ok(());
        }
        let take = (n as u64).min(end - pos) as usize;
        if buf.is_empty() {
            off = pos;
        }
        buf.extend_from_slice(&read[..take]);
        seg.advance(pos + take as u64);
        if buf.len() >= 512 * 1024 {
            let data = buf.split().freeze();
            let len = data.len() as u64;
            if tx.send(writer::Chunk { seg: seg.clone(), off, data }).await.is_err() {
                return Err(Fail::Disk("the disk stopped accepting writes".into()));
            }
            off += len;
        }
        if take < n || pos + take as u64 >= end {
            break Ok(());
        }
    };
    if !buf.is_empty() {
        let _ = tx.send(writer::Chunk { seg: seg.clone(), off, data: buf.freeze() }).await;
    }
    drop(stream);
    drop(c);
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ftp_links() {
        let s = spot("ftp://bob:p%40ss@files.example.com:2121/pub/My%20File.iso").unwrap();
        assert_eq!(s.addr, "files.example.com:2121");
        assert_eq!(s.user, "bob");
        assert_eq!(s.pass, "p@ss");
        assert_eq!(s.path, "/pub/My File.iso");
        let a = spot("ftp://ftp.example.org/a.zip").unwrap();
        assert_eq!(a.user, "anonymous");
        assert_eq!(a.addr, "ftp.example.org:21");
    }
}
