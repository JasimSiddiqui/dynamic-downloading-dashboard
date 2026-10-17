use super::{resolve, text, Key, Piece, Track};
use crate::engine::{Fail, Job};
use m3u8_rs::{AlternativeMediaType, KeyMethod, MediaPlaylist, Playlist};
use reqwest::header::HeaderMap;
use serde::Serialize;

#[derive(Serialize, Clone, Debug)]
pub struct Choice {
    pub id: u64,
    pub height: Option<u32>,
    pub label: String,
}

pub struct Plan {
    pub video: Track,
    pub audio: Option<Track>,
}

async fn load(job: &Job, url: &str, headers: &HeaderMap) -> Result<(String, Playlist), Fail> {
    let (base, body) = text(job, url, headers).await?;
    let pl = m3u8_rs::parse_playlist_res(body.as_bytes()).map_err(|_| Fail::Other("That stream's playlist couldn't be read".into()))?;
    Ok((base, pl))
}

pub async fn choices(job: &Job, url: &str, headers: &HeaderMap) -> Result<Vec<Choice>, Fail> {
    let (_, pl) = load(job, url, headers).await?;
    let Playlist::MasterPlaylist(m) = pl else { return Ok(Vec::new()) };
    let mut out: Vec<Choice> = m
        .variants
        .iter()
        .filter(|v| !v.is_i_frame)
        .map(|v| {
            let height = v.resolution.map(|r| r.height as u32);
            Choice {
                id: v.bandwidth,
                height,
                label: match height {
                    Some(h) => format!("{h}p"),
                    None => format!("{:.1} Mb/s", v.bandwidth as f64 / 1e6),
                },
            }
        })
        .collect();
    out.sort_by(|a, b| b.id.cmp(&a.id));
    out.dedup_by(|a, b| a.label == b.label);
    Ok(out)
}

pub async fn plan(job: &Job, url: &str, headers: &HeaderMap, want: Option<u64>) -> Result<Plan, Fail> {
    let (base, pl) = load(job, url, headers).await?;
    match pl {
        Playlist::MediaPlaylist(m) => Ok(Plan { video: track(&base, &m)?, audio: None }),
        Playlist::MasterPlaylist(m) => {
            let mut vs: Vec<_> = m.variants.iter().filter(|v| !v.is_i_frame).collect();
            vs.sort_by_key(|v| std::cmp::Reverse(v.bandwidth));
            let v = want
                .and_then(|w| vs.iter().find(|v| v.bandwidth <= w).copied())
                .or(vs.first().copied())
                .ok_or_else(|| Fail::Other("That stream has no video in it".into()))?;
            let vurl = resolve(&base, &v.uri);
            let (vbase, vpl) = load(job, &vurl, headers).await?;
            let Playlist::MediaPlaylist(vm) = vpl else { return Err(Fail::Other("That stream's playlist is nested too deep".into())) };
            let video = track(&vbase, &vm)?;
            let audio_uri = v.audio.as_ref().and_then(|g| {
                let group: Vec<_> = m
                    .alternatives
                    .iter()
                    .filter(|a| a.media_type == AlternativeMediaType::Audio && &a.group_id == g && a.uri.is_some())
                    .collect();
                group.iter().find(|a| a.default).or(group.first()).and_then(|a| a.uri.clone())
            });
            let audio = match audio_uri {
                Some(u) => {
                    let (abase, apl) = load(job, &resolve(&base, &u), headers).await?;
                    match apl {
                        Playlist::MediaPlaylist(am) => Some(track(&abase, &am)?),
                        _ => None,
                    }
                }
                None => None,
            };
            Ok(Plan { video, audio })
        }
    }
}

fn parse_iv(iv: &str) -> Option<[u8; 16]> {
    let h = iv.trim_start_matches("0x").trim_start_matches("0X");
    let v = hex::decode(format!("{h:0>32}")).ok()?;
    v.try_into().ok()
}

pub fn track(base: &str, m: &MediaPlaylist) -> Result<Track, Fail> {
    if !m.end_list && m.playlist_type != Some(m3u8_rs::MediaPlaylistType::Vod) {
        return Err(Fail::Other("Live streams can't be saved yet".into()));
    }
    let mut key: Option<m3u8_rs::Key> = None;
    let mut pieces = Vec::with_capacity(m.segments.len());
    let mut init = None;
    let mut next_off: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
    for (i, s) in m.segments.iter().enumerate() {
        if let Some(k) = &s.key {
            key = Some(k.clone());
        }
        let url = resolve(base, &s.uri);
        let range = s.byte_range.as_ref().map(|r| {
            let off = r.offset.unwrap_or_else(|| *next_off.get(&url).unwrap_or(&0));
            next_off.insert(url.clone(), off + r.length);
            (off, r.length)
        });
        if init.is_none() {
            if let Some(map) = &s.map {
                init = Some(Piece {
                    url: resolve(base, &map.uri),
                    range: map.byte_range.as_ref().map(|r| (r.offset.unwrap_or(0), r.length)),
                    key: None,
                });
            }
        }
        let k = match &key {
            None => None,
            Some(k) => match &k.method {
                KeyMethod::None => None,
                KeyMethod::AES128 if k.keyformat.as_deref().is_none_or(|f| f == "identity") => {
                    let uri = k.uri.as_deref().ok_or_else(|| Fail::Other("This stream's key is missing".into()))?;
                    let iv = match k.iv.as_deref() {
                        Some(iv) => parse_iv(iv).ok_or_else(|| Fail::Other("This stream's key is malformed".into()))?,
                        None => ((m.media_sequence + i as u64) as u128).to_be_bytes(),
                    };
                    Some(Key { uri: resolve(base, uri), iv })
                }
                _ => return Err(Fail::Other("This stream is DRM-protected, so it can't be saved".into())),
            },
        };
        pieces.push(Piece { url, range, key: k });
    }
    if pieces.is_empty() {
        return Err(Fail::Other("That stream is empty".into()));
    }
    let ext = if init.is_some() { "mp4" } else { "ts" };
    Ok(Track { init, pieces, whole: None, ext })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn media(src: &str) -> MediaPlaylist {
        match m3u8_rs::parse_playlist_res(src.as_bytes()).unwrap() {
            Playlist::MediaPlaylist(m) => m,
            _ => panic!("not media"),
        }
    }

    #[test]
    fn plain_and_encrypted() {
        let m = media(
            "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:5\n#EXTINF:4,\na.ts\n#EXT-X-KEY:METHOD=AES-128,URI=\"k.bin\"\n#EXTINF:4,\nb.ts\n#EXT-X-KEY:METHOD=AES-128,URI=\"k2.bin\",IV=0x0000000000000000000000000000000A\n#EXTINF:4,\nc.ts\n#EXT-X-ENDLIST\n",
        );
        let t = track("https://h.com/p/index.m3u8", &m).unwrap();
        assert_eq!(t.ext, "ts");
        assert_eq!(t.pieces.len(), 3);
        assert!(t.pieces[0].key.is_none());
        let k = t.pieces[1].key.as_ref().unwrap();
        assert_eq!(k.uri, "https://h.com/p/k.bin");
        assert_eq!(k.iv, 6u128.to_be_bytes());
        assert_eq!(t.pieces[2].key.as_ref().unwrap().iv, 10u128.to_be_bytes());
    }

    #[test]
    fn rejects_drm_and_live() {
        let drm = media("#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"skd://x\",KEYFORMAT=\"com.apple.streamingkeydelivery\"\n#EXTINF:4,\na.ts\n#EXT-X-ENDLIST\n");
        assert!(matches!(track("https://h.com/i.m3u8", &drm), Err(Fail::Other(m)) if m.contains("DRM")));
        let live = media("#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXTINF:4,\na.ts\n");
        assert!(track("https://h.com/i.m3u8", &live).is_err());
    }

    #[test]
    fn fmp4_with_byte_ranges() {
        let m = media("#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MAP:URI=\"v.mp4\",BYTERANGE=\"700@0\"\n#EXTINF:4,\n#EXT-X-BYTERANGE:1000@700\nv.mp4\n#EXTINF:4,\n#EXT-X-BYTERANGE:500\nv.mp4\n#EXT-X-ENDLIST\n");
        let t = track("https://h.com/i.m3u8", &m).unwrap();
        assert_eq!(t.ext, "mp4");
        assert_eq!(t.init.unwrap().range, Some((0, 700)));
        assert_eq!(t.pieces[0].range, Some((700, 1000)));
        assert_eq!(t.pieces[1].range, Some((1700, 500)));
    }
}
