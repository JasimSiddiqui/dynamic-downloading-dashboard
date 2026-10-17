use crate::engine::Fail;
use librqbit::api::TorrentIdOrHash;
use librqbit::dht::Id20;
use librqbit::{AddTorrent, AddTorrentOptions, AddTorrentResponse, ListenerMode, ListenerOptions, Session, SessionOptions, SessionPersistenceConfig};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::OnceCell;

#[derive(Serialize, Clone, Debug)]
pub struct File {
    pub name: String,
    pub size: u64,
}

#[derive(Clone, Debug)]
pub struct Meta {
    pub name: String,
    pub hash: String,
    pub files: Vec<File>,
    pub bytes: bytes::Bytes,
}

pub struct Stats {
    pub done: u64,
    pub total: u64,
    pub down: f64,
    pub uploaded: u64,
    pub finished: bool,
    pub error: Option<String>,
}

pub struct Torrents {
    session: OnceCell<Arc<Session>>,
    dir: PathBuf,
    out: PathBuf,
}

fn fail(e: anyhow::Error) -> Fail {
    Fail::Other(format!("Torrent error: {e}"))
}

impl Torrents {
    pub fn new(dir: PathBuf, out: PathBuf) -> Self {
        Self { session: OnceCell::new(), dir, out }
    }

    pub async fn session(&self) -> Result<Arc<Session>, Fail> {
        self.session
            .get_or_try_init(|| async {
                std::fs::create_dir_all(&self.dir).map_err(|e| Fail::Disk(e.to_string()))?;
                let opts = SessionOptions {
                    persistence: Some(SessionPersistenceConfig::Json { folder: Some(self.dir.join("session")) }),
                    fastresume: true,
                    listen: Some(ListenerOptions {
                        mode: ListenerMode::TcpAndUtp,
                        listen_addr: (std::net::Ipv6Addr::UNSPECIFIED, 0).into(),
                        enable_upnp_port_forwarding: true,
                        ..Default::default()
                    }),
                    client_name_and_version: Some(format!("DDD/{}", env!("CARGO_PKG_VERSION"))),
                    ..Default::default()
                };
                Session::new_with_opts(self.out.clone(), opts).await.map_err(fail)
            })
            .await
            .cloned()
    }

    fn add_source(src: &str) -> Result<AddTorrent<'static>, Fail> {
        if src.starts_with("magnet:") || src.starts_with("http://") || src.starts_with("https://") {
            Ok(AddTorrent::from_url(src.to_string()))
        } else {
            std::fs::read(src).map(AddTorrent::from_bytes).map_err(|e| Fail::Disk(e.to_string()))
        }
    }

    pub async fn inspect(&self, src: &str) -> Result<Meta, Fail> {
        let s = self.session().await?;
        let add = Self::add_source(src)?;
        let opts = AddTorrentOptions { list_only: true, ..Default::default() };
        let r = tokio::time::timeout(std::time::Duration::from_secs(90), s.add_torrent(add, Some(opts)))
            .await
            .map_err(|_| Fail::Other("Couldn't find anyone sharing that torrent".into()))?
            .map_err(fail)?;
        match r {
            AddTorrentResponse::ListOnly(l) => {
                let files = l
                    .info
                    .iter_file_details()
                    .map(|f| File { name: f.filename.to_pathbuf().to_string_lossy().into_owned(), size: f.len })
                    .collect();
                Ok(Meta {
                    name: l.info.name().map(|n| n.into_owned()).unwrap_or_else(|| "Torrent".into()),
                    hash: l.info_hash.as_string(),
                    files,
                    bytes: l.torrent_bytes,
                })
            }
            AddTorrentResponse::AlreadyManaged(_, h) | AddTorrentResponse::Added(_, h) => Err(Fail::Other(format!(
                "{} is already in the list",
                h.name().unwrap_or_else(|| "That torrent".into())
            ))),
        }
    }

    pub async fn add(&self, torrent: &[u8], dir: &Path, only: Option<Vec<usize>>) -> Result<String, Fail> {
        let s = self.session().await?;
        let opts = AddTorrentOptions {
            only_files: only,
            overwrite: true,
            output_folder: Some(dir.to_string_lossy().into_owned()),
            ..Default::default()
        };
        let r = s.add_torrent(AddTorrent::from_bytes(bytes::Bytes::copy_from_slice(torrent)), Some(opts)).await.map_err(fail)?;
        let h = r.into_handle().ok_or_else(|| Fail::Other("The torrent didn't start".into()))?;
        Ok(h.info_hash().as_string())
    }

    fn id(hash: &str) -> Option<TorrentIdOrHash> {
        Id20::from_str(hash).ok().map(TorrentIdOrHash::from)
    }

    pub fn has(&self, hash: &str) -> bool {
        self.stats(hash).is_some()
    }

    pub fn stats(&self, hash: &str) -> Option<Stats> {
        let s = self.session.get()?;
        let h = s.get(Self::id(hash)?)?;
        let st = h.stats();
        Some(Stats {
            done: st.progress_bytes,
            total: st.total_bytes,
            down: st.live.as_ref().map(|l| l.download_speed.mbps * 1_048_576.0).unwrap_or(0.0),
            uploaded: st.uploaded_bytes,
            finished: st.finished,
            error: st.error,
        })
    }

    pub async fn pause(&self, hash: &str) -> Result<(), Fail> {
        let s = self.session().await?;
        if let Some(h) = Self::id(hash).and_then(|id| s.get(id)) {
            if !matches!(h.stats().state, librqbit::TorrentStatsState::Paused) {
                s.pause(&h).await.map_err(fail)?;
            }
        }
        Ok(())
    }

    pub async fn resume(&self, hash: &str) -> Result<(), Fail> {
        let s = self.session().await?;
        if let Some(h) = Self::id(hash).and_then(|id| s.get(id)) {
            if matches!(h.stats().state, librqbit::TorrentStatsState::Paused) {
                s.unpause(&h).await.map_err(fail)?;
            }
        }
        Ok(())
    }

    pub async fn remove(&self, hash: &str, files: bool) -> Result<(), Fail> {
        let s = self.session().await?;
        if let Some(id) = Self::id(hash) {
            if s.get(id).is_some() {
                s.delete(id, files).await.map_err(fail)?;
            }
        }
        Ok(())
    }
}
