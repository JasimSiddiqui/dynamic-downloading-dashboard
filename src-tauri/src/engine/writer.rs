use super::splitter::Seg;
use bytes::Bytes;
use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::mpsc;

pub struct Chunk {
    pub seg: Arc<Seg>,
    pub off: u64,
    pub data: Bytes,
}

pub type Tx = mpsc::Sender<Chunk>;

pub struct Writer {
    tx: Option<Tx>,
    thread: Option<std::thread::JoinHandle<io::Result<()>>>,
}

impl Writer {
    pub fn open(path: &Path, size: Option<u64>, truncate: bool) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).write(true).create(true).truncate(truncate).open(path)?;
        if let Some(size) = size {
            if file.metadata()?.len() != size {
                file.set_len(size)?;
            }
        }
        let (tx, mut rx) = mpsc::channel::<Chunk>(48);
        let thread = std::thread::Builder::new().name("ddd-writer".into()).spawn(move || {
            while let Some(c) = rx.blocking_recv() {
                write_at(&file, &c.data, c.off)?;
                c.seg.wrote(c.off + c.data.len() as u64);
            }
            Ok(())
        })?;
        Ok(Self { tx: Some(tx), thread: Some(thread) })
    }

    pub fn tx(&self) -> Tx {
        self.tx.clone().expect("writer open")
    }

    pub fn alive(&self) -> bool {
        self.thread.as_ref().is_some_and(|t| !t.is_finished())
    }

    pub async fn finish(mut self) -> io::Result<()> {
        self.tx.take();
        let t = self.thread.take();
        tokio::task::spawn_blocking(move || match t {
            Some(t) => t.join().unwrap_or_else(|_| Err(io::Error::other("writer panicked"))),
            None => Ok(()),
        })
        .await
        .map_err(io::Error::other)?
    }
}

#[cfg(windows)]
fn write_at(file: &File, mut buf: &[u8], mut off: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        let n = file.seek_write(buf, off)?;
        if n == 0 {
            return Err(io::ErrorKind::WriteZero.into());
        }
        buf = &buf[n..];
        off += n as u64;
    }
    Ok(())
}

#[cfg(unix)]
fn write_at(file: &File, buf: &[u8], off: u64) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    file.write_all_at(buf, off)
}
