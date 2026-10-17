use super::{resolve, text, Piece, Track};
use crate::engine::{Fail, Job};
use quick_xml::events::Event;
use reqwest::header::HeaderMap;

#[derive(Debug, Default, Clone)]
struct Node {
    name: String,
    attrs: Vec<(String, String)>,
    kids: Vec<Node>,
    text: String,
}

impl Node {
    fn attr(&self, k: &str) -> Option<&str> {
        self.attrs.iter().find(|(a, _)| a == k).map(|(_, v)| v.as_str())
    }
    fn kid(&self, n: &str) -> Option<&Node> {
        self.kids.iter().find(|k| k.name == n)
    }
    fn all<'a>(&'a self, n: &'a str) -> impl Iterator<Item = &'a Node> + 'a {
        self.kids.iter().filter(move |k| k.name == n)
    }
}

fn parse(xml: &str) -> Result<Node, Fail> {
    let bad = || Fail::Other("That stream's manifest couldn't be read".into());
    let mut r = quick_xml::Reader::from_str(xml);
    r.config_mut().trim_text(true);
    let mut stack: Vec<Node> = vec![Node::default()];
    let open = |e: &quick_xml::events::BytesStart| Node {
        name: String::from_utf8_lossy(e.local_name().as_ref()).into_owned(),
        attrs: e
            .attributes()
            .flatten()
            .map(|a| {
                (
                    String::from_utf8_lossy(a.key.local_name().as_ref()).into_owned(),
                    a.unescape_value().map(|v| v.into_owned()).unwrap_or_default(),
                )
            })
            .collect(),
        ..Default::default()
    };
    loop {
        match r.read_event().map_err(|_| bad())? {
            Event::Start(e) => stack.push(open(&e)),
            Event::Empty(e) => {
                let n = open(&e);
                stack.last_mut().ok_or_else(bad)?.kids.push(n);
            }
            Event::Text(t) => {
                if let Some(top) = stack.last_mut() {
                    top.text.push_str(&t.unescape().map(|v| v.into_owned()).unwrap_or_default());
                }
            }
            Event::End(_) => {
                let n = stack.pop().ok_or_else(bad)?;
                stack.last_mut().ok_or_else(bad)?.kids.push(n);
            }
            Event::Eof => break,
            _ => {}
        }
    }
    stack.pop().and_then(|root| root.kids.into_iter().find(|k| k.name == "MPD")).ok_or_else(bad)
}

pub fn duration(s: &str) -> Option<f64> {
    let s = s.strip_prefix('P')?;
    let (date, time) = s.split_once('T').unwrap_or((s, ""));
    let mut total = 0.0;
    let mut num = String::new();
    for (part, is_time) in [(date, false), (time, true)] {
        for c in part.chars() {
            if c.is_ascii_digit() || c == '.' {
                num.push(c);
                continue;
            }
            let v: f64 = num.parse().ok()?;
            num.clear();
            total += v * match (c, is_time) {
                ('Y', false) => 31_536_000.0,
                ('M', false) => 2_592_000.0,
                ('W', false) => 604_800.0,
                ('D', false) => 86_400.0,
                ('H', true) => 3600.0,
                ('M', true) => 60.0,
                ('S', true) => 1.0,
                _ => return None,
            };
        }
    }
    Some(total)
}

fn fill(tpl: &str, id: &str, bw: &str, number: Option<u64>, time: Option<u64>) -> String {
    let mut out = String::with_capacity(tpl.len() + 16);
    let mut rest = tpl;
    while let Some(i) = rest.find('$') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        let Some(j) = after.find('$') else {
            out.push_str(&rest[i..]);
            return out;
        };
        let tag = &after[..j];
        let (name, fmt) = tag.split_once('%').unwrap_or((tag, ""));
        let width: usize = fmt.trim_start_matches('0').trim_end_matches('d').parse().unwrap_or(0);
        let num = |v: u64| format!("{v:0width$}");
        match name {
            "" => out.push('$'),
            "RepresentationID" => out.push_str(id),
            "Bandwidth" => out.push_str(bw),
            "Number" => out.push_str(&num(number.unwrap_or(0))),
            "Time" => out.push_str(&num(time.unwrap_or(0))),
            _ => {
                out.push('$');
                out.push_str(tag);
                out.push('$');
            }
        }
        rest = &after[j + 1..];
    }
    out.push_str(rest);
    out
}

fn base_of(node: &Node, parent: &str) -> String {
    match node.kid("BaseURL").map(|b| b.text.trim()).filter(|t| !t.is_empty()) {
        Some(b) => resolve(parent, b),
        None => parent.to_string(),
    }
}

fn merged<'a>(levels: &[&'a Node], name: &str) -> Option<Vec<&'a Node>> {
    let v: Vec<&Node> = levels.iter().filter_map(|l| l.kid(name)).collect();
    (!v.is_empty()).then_some(v)
}

fn pick<'a>(nodes: &[&'a Node], attr: &str) -> Option<&'a str> {
    nodes.iter().rev().find_map(|n| n.attr(attr))
}

fn rep_track(levels: [&Node; 3], base: &str, period_secs: f64) -> Result<Track, Fail> {
    let [period, set, rep] = levels;
    let id = rep.attr("id").unwrap_or("");
    let bw = rep.attr("bandwidth").unwrap_or("0");
    let mime = rep.attr("mimeType").or(set.attr("mimeType")).unwrap_or("");
    let ext = if mime.contains("webm") { "webm" } else { "mp4" };
    if let Some(tpls) = merged(&[period, set, rep], "SegmentTemplate") {
        let timescale: f64 = pick(&tpls, "timescale").and_then(|v| v.parse().ok()).unwrap_or(1.0);
        let start: u64 = pick(&tpls, "startNumber").and_then(|v| v.parse().ok()).unwrap_or(1);
        let media = pick(&tpls, "media").ok_or_else(|| Fail::Other("That stream's manifest has no segment pattern".into()))?;
        let init = pick(&tpls, "initialization").map(|i| Piece { url: resolve(base, &fill(i, id, bw, None, None)), range: None, key: None });
        let mut pieces = Vec::new();
        if let Some(tl) = tpls.iter().rev().find_map(|t| t.kid("SegmentTimeline")) {
            let mut t = 0u64;
            let mut n = start;
            let ss: Vec<&Node> = tl.all("S").collect();
            for (i, s) in ss.iter().enumerate() {
                if let Some(st) = s.attr("t").and_then(|v| v.parse().ok()) {
                    t = st;
                }
                let d: u64 = s.attr("d").and_then(|v| v.parse().ok()).unwrap_or(0);
                if d == 0 {
                    continue;
                }
                let r: i64 = s.attr("r").and_then(|v| v.parse().ok()).unwrap_or(0);
                let count = if r >= 0 {
                    r as u64 + 1
                } else {
                    let end = ss.get(i + 1).and_then(|x| x.attr("t")).and_then(|v| v.parse::<u64>().ok()).unwrap_or((period_secs * timescale) as u64);
                    end.saturating_sub(t).div_ceil(d)
                };
                for _ in 0..count {
                    pieces.push(Piece { url: resolve(base, &fill(media, id, bw, Some(n), Some(t))), range: None, key: None });
                    t += d;
                    n += 1;
                }
            }
        } else {
            let d: f64 = pick(&tpls, "duration").and_then(|v| v.parse().ok()).ok_or_else(|| Fail::Other("That stream's manifest has no segment length".into()))?;
            let count = ((period_secs * timescale) / d).ceil().max(1.0) as u64;
            for k in 0..count {
                pieces.push(Piece { url: resolve(base, &fill(media, id, bw, Some(start + k), Some((k as f64 * d) as u64))), range: None, key: None });
            }
        }
        return Ok(Track { init, pieces, whole: None, ext });
    }
    if let Some(list) = rep.kid("SegmentList").or(set.kid("SegmentList")) {
        let range = |n: &Node, a: &str| {
            n.attr(a).and_then(|r| r.split_once('-')).and_then(|(s, e)| Some((s.parse::<u64>().ok()?, e.parse::<u64>().ok()?))).map(|(s, e)| (s, e - s + 1))
        };
        let init = list.kid("Initialization").map(|i| Piece {
            url: resolve(base, i.attr("sourceURL").unwrap_or("")),
            range: range(i, "range"),
            key: None,
        });
        let pieces = list
            .all("SegmentURL")
            .map(|s| Piece { url: resolve(base, s.attr("media").unwrap_or("")), range: range(s, "mediaRange"), key: None })
            .collect();
        return Ok(Track { init, pieces, whole: None, ext });
    }
    Ok(Track { init: None, pieces: Vec::new(), whole: Some(base.to_string()), ext })
}

pub struct Plan {
    pub video: Option<Track>,
    pub audio: Option<Track>,
    pub heights: Vec<u32>,
}

pub fn plan_from(xml: &str, url: &str, want: Option<u32>) -> Result<Plan, Fail> {
    let mpd = parse(xml)?;
    if mpd.attr("type") == Some("dynamic") {
        return Err(Fail::Other("Live streams can't be saved yet".into()));
    }
    let period = mpd.kid("Period").ok_or_else(|| Fail::Other("That stream's manifest is empty".into()))?;
    let secs = period
        .attr("duration")
        .or(mpd.attr("mediaPresentationDuration"))
        .and_then(duration)
        .unwrap_or(0.0);
    let mbase = base_of(&mpd, url);
    let pbase = base_of(period, &mbase);
    let mut videos = Vec::new();
    let mut audios = Vec::new();
    let mut drm = false;
    for set in period.all("AdaptationSet") {
        let sbase = base_of(set, &pbase);
        for rep in set.all("Representation") {
            if set.kid("ContentProtection").is_some() || rep.kid("ContentProtection").is_some() {
                drm = true;
                continue;
            }
            let kind = set
                .attr("contentType")
                .map(str::to_string)
                .or_else(|| rep.attr("mimeType").or(set.attr("mimeType")).and_then(|m| m.split('/').next()).map(str::to_string))
                .unwrap_or_default();
            let bw: u64 = rep.attr("bandwidth").and_then(|v| v.parse().ok()).unwrap_or(0);
            let h: Option<u32> = rep.attr("height").or(set.attr("height")).and_then(|v| v.parse().ok());
            let entry = (bw, h, set, rep, base_of(rep, &sbase));
            match kind.as_str() {
                "video" => videos.push(entry),
                "audio" => audios.push(entry),
                _ => {}
            }
        }
    }
    if videos.is_empty() && audios.is_empty() {
        return Err(Fail::Other(if drm { "This stream is DRM-protected, so it can't be saved".into() } else { "That stream has nothing to save".into() }));
    }
    let mut heights: Vec<u32> = videos.iter().filter_map(|v| v.1).collect();
    heights.sort_unstable_by(|a, b| b.cmp(a));
    heights.dedup();
    videos.sort_by_key(|v| (std::cmp::Reverse(v.1.unwrap_or(0)), std::cmp::Reverse(v.0)));
    let v = want.and_then(|w| videos.iter().find(|v| v.1.unwrap_or(0) <= w)).or(videos.first());
    audios.sort_by_key(|a| std::cmp::Reverse(a.0));
    let video = v.map(|v| rep_track([period, v.2, v.3], &v.4, secs)).transpose()?;
    let audio = audios.first().map(|a| rep_track([period, a.2, a.3], &a.4, secs)).transpose()?;
    Ok(Plan { video, audio, heights })
}

pub async fn plan(job: &Job, url: &str, headers: &HeaderMap, want: Option<u32>) -> Result<Plan, Fail> {
    let (final_url, xml) = text(job, url, headers).await?;
    plan_from(&xml, &final_url, want)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MPD: &str = r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT0M10.0S">
  <BaseURL>https://cdn.example.com/v/</BaseURL>
  <Period>
    <AdaptationSet contentType="video" mimeType="video/mp4">
      <SegmentTemplate timescale="1000" initialization="$RepresentationID$/init.mp4" media="$RepresentationID$/$Number%05d$.m4s" startNumber="1" duration="4000"/>
      <Representation id="720" bandwidth="2000000" height="720"/>
      <Representation id="1080" bandwidth="5000000" height="1080"/>
    </AdaptationSet>
    <AdaptationSet contentType="audio" mimeType="audio/mp4">
      <SegmentTemplate timescale="48000" initialization="a/init.mp4" media="a/t$Time$.m4s">
        <SegmentTimeline><S t="0" d="96000" r="2"/><S d="48000"/></SegmentTimeline>
      </SegmentTemplate>
      <Representation id="aud" bandwidth="128000"/>
    </AdaptationSet>
  </Period>
</MPD>"#;

    #[test]
    fn durations() {
        assert_eq!(duration("PT1H2M3.5S"), Some(3723.5));
        assert_eq!(duration("P1DT1S"), Some(86401.0));
        assert_eq!(duration("bad"), None);
    }

    #[test]
    fn templates() {
        assert_eq!(fill("$RepresentationID$/$Number%05d$.m4s", "v1", "9", Some(42), None), "v1/00042.m4s");
        assert_eq!(fill("t$Time$-$Bandwidth$$$", "x", "128", None, Some(96000)), "t96000-128$");
    }

    #[test]
    fn plans_best_and_chosen() {
        let p = plan_from(MPD, "https://site.com/manifest.mpd", None).unwrap();
        assert_eq!(p.heights, vec![1080, 720]);
        let v = p.video.unwrap();
        assert_eq!(v.init.unwrap().url, "https://cdn.example.com/v/1080/init.mp4");
        assert_eq!(v.pieces.len(), 3);
        assert_eq!(v.pieces[2].url, "https://cdn.example.com/v/1080/00003.m4s");
        let a = p.audio.unwrap();
        assert_eq!(a.pieces.len(), 4);
        assert_eq!(a.pieces[3].url, "https://cdn.example.com/v/a/t288000.m4s");
        let p = plan_from(MPD, "https://site.com/manifest.mpd", Some(720)).unwrap();
        assert!(p.video.unwrap().pieces[0].url.contains("/720/"));
    }

    #[test]
    fn drm_refused() {
        let x = r#"<MPD type="static" mediaPresentationDuration="PT10S"><Period><AdaptationSet contentType="video"><ContentProtection schemeIdUri="urn:uuid:edef8ba9-79d6-4ace-a3c8-27dcd51d21ed"/><Representation id="v" bandwidth="1"/></AdaptationSet></Period></MPD>"#;
        assert!(matches!(plan_from(x, "https://a.com/m.mpd", None), Err(Fail::Other(m)) if m.contains("DRM")));
    }
}
