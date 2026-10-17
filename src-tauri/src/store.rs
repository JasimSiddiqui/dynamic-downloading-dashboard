use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    File,
    Stream,
    Video,
    Torrent,
    Ftp,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Queued,
    Running,
    Paused,
    Seeding,
    Done,
    Failed,
    Expired,
}

#[derive(Serialize, Deserialize, Clone, Default, Debug)]
pub struct Resume {
    pub size: u64,
    pub holes: Vec<(u64, u64)>,
    pub etag: Option<String>,
    pub modified: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "via", rename_all = "lowercase")]
pub enum Media {
    Hls { variant: Option<u64> },
    Dash { height: Option<u32> },
    Ytdlp { height: Option<u32>, audio: bool },
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Torrent {
    pub hash: String,
    pub files: Option<Vec<usize>>,
    #[serde(default)]
    pub source: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Item {
    pub id: u64,
    pub kind: Kind,
    pub status: Status,
    pub name: String,
    pub dir: PathBuf,
    #[serde(default)]
    pub path: Option<PathBuf>,
    pub urls: Vec<String>,
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    #[serde(default)]
    pub size: Option<u64>,
    #[serde(default)]
    pub done: u64,
    pub created: u64,
    #[serde(default)]
    pub finished: Option<u64>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub resume: Option<Resume>,
    #[serde(default)]
    pub media: Option<Media>,
    #[serde(default)]
    pub torrent: Option<Torrent>,
    #[serde(default)]
    pub uploaded: u64,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct View {
    pub id: u64,
    pub kind: Kind,
    pub status: Status,
    pub name: String,
    pub host: String,
    pub path: Option<String>,
    pub size: Option<u64>,
    pub done: u64,
    pub created: u64,
    pub finished: Option<u64>,
    pub error: Option<String>,
    pub sources: Vec<String>,
}

impl Item {
    pub fn view(&self) -> View {
        View {
            id: self.id,
            kind: self.kind,
            status: self.status,
            name: self.name.clone(),
            host: self.host(),
            path: self.path.as_ref().map(|p| p.to_string_lossy().into_owned()),
            size: self.size,
            done: self.done,
            created: self.created,
            finished: self.finished,
            error: self.error.clone(),
            sources: self.urls.clone(),
        }
    }

    pub fn host(&self) -> String {
        if self.kind == Kind::Torrent {
            return String::new();
        }
        self.urls
            .first()
            .and_then(|u| url::Url::parse(u).ok())
            .and_then(|u| u.host_str().map(|h| h.trim_start_matches("www.").to_string()))
            .unwrap_or_default()
    }

    pub fn active(&self) -> bool {
        matches!(self.status, Status::Running | Status::Seeding)
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    pub dir: PathBuf,
    pub connections: u32,
    pub at_once: u32,
    pub speed_cap: u64,
    pub seed_ratio: f64,
    pub catch: bool,
    pub magnets: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            dir: downloads_dir(),
            connections: 16,
            at_once: 3,
            speed_cap: 0,
            seed_ratio: 1.0,
            catch: true,
            magnets: false,
        }
    }
}

pub fn downloads_dir() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .map(|h| PathBuf::from(h).join("Downloads"))
        .filter(|p| p.is_dir())
        .unwrap_or_else(|| std::env::temp_dir())
}

#[derive(Serialize, Deserialize, Default)]
pub struct Saved {
    #[serde(default)]
    pub items: Vec<Item>,
    #[serde(default)]
    pub settings: Settings,
    #[serde(default)]
    pub next_id: u64,
}

pub fn load(path: &Path) -> Saved {
    std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

pub fn save(path: &Path, saved: &Saved) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_vec(saved)?)?;
    std::fs::rename(tmp, path)
}

pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let mut saved = Saved::default();
        saved.items.push(Item {
            id: 7,
            kind: Kind::File,
            status: Status::Paused,
            name: "a.bin".into(),
            dir: dir.path().into(),
            path: None,
            urls: vec!["https://example.com/a.bin".into()],
            headers: vec![("Cookie".into(), "x=1".into())],
            size: Some(10),
            done: 4,
            created: 1,
            finished: None,
            error: None,
            resume: Some(Resume { size: 10, holes: vec![(4, 10)], etag: Some("e".into()), modified: None }),
            media: Some(Media::Ytdlp { height: Some(720), audio: false }),
            torrent: None,
            uploaded: 0,
        });
        save(&path, &saved).unwrap();
        let back = load(&path);
        assert_eq!(back.items.len(), 1);
        assert_eq!(back.items[0].resume.as_ref().unwrap().holes, vec![(4, 10)]);
        assert_eq!(back.items[0].host(), "example.com");
    }
}
