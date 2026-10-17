use crate::{curl, grab};

#[derive(Debug, Clone, PartialEq)]
pub enum Catch {
    Link(String),
    Magnet(String),
    Curl(String),
    Many(usize),
}

const KNOWN_VIDEO: [&str; 18] = [
    "youtube.com", "youtu.be", "vimeo.com", "twitch.tv", "tiktok.com", "x.com", "twitter.com", "instagram.com",
    "facebook.com", "fb.watch", "reddit.com", "dailymotion.com", "soundcloud.com", "bilibili.com", "streamable.com",
    "bandcamp.com", "rumble.com", "kick.com",
];

pub fn video_host(url: &str) -> bool {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_ascii_lowercase))
        .is_some_and(|h| KNOWN_VIDEO.iter().any(|k| h == *k || h.ends_with(&format!(".{k}"))))
}

pub fn classify(text: &str) -> Option<Catch> {
    let t = text.trim();
    if t.is_empty() || t.len() > 200_000 {
        return None;
    }
    if t.starts_with("curl ") || t.starts_with("curl.exe ") {
        return curl::parse(t).map(|_| Catch::Curl(t.to_string()));
    }
    if !t.contains(char::is_whitespace) {
        if t.starts_with("magnet:?") {
            return Some(Catch::Magnet(t.to_string()));
        }
        if t.starts_with("http://") || t.starts_with("https://") || t.starts_with("ftp://") {
            return url::Url::parse(t).ok().map(|_| Catch::Link(t.to_string()));
        }
        return None;
    }
    let files = grab::extract(t, None).len();
    (files >= 2).then_some(Catch::Many(files))
}

#[cfg(windows)]
pub fn watch(on_text: impl Fn(String) + Send + 'static) {
    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows_sys::Win32::System::DataExchange::{AddClipboardFormatListener, CloseClipboard, GetClipboardData, OpenClipboard};
    use windows_sys::Win32::System::Memory::{GlobalLock, GlobalUnlock};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, RegisterClassW, TranslateMessage, HWND_MESSAGE, MSG,
        WM_CLIPBOARDUPDATE, WNDCLASSW,
    };

    unsafe extern "system" fn proc(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
        DefWindowProcW(h, m, w, l)
    }

    fn read() -> Option<String> {
        unsafe {
            let mut opened = false;
            for _ in 0..10 {
                if OpenClipboard(std::ptr::null_mut()) != 0 {
                    opened = true;
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            if !opened {
                return None;
            }
            let h = GetClipboardData(13);
            let mut out = None;
            if !h.is_null() {
                let p = GlobalLock(h) as *const u16;
                if !p.is_null() {
                    let mut n = 0;
                    while *p.add(n) != 0 && n < 400_000 {
                        n += 1;
                    }
                    out = Some(String::from_utf16_lossy(std::slice::from_raw_parts(p, n)));
                    GlobalUnlock(h);
                }
            }
            CloseClipboard();
            out
        }
    }

    std::thread::Builder::new()
        .name("ddd-clipboard".into())
        .spawn(move || unsafe {
            let name: Vec<u16> = "DDDClipboard\0".encode_utf16().collect();
            let wc = WNDCLASSW {
                lpfnWndProc: Some(proc),
                lpszClassName: name.as_ptr(),
                ..std::mem::zeroed()
            };
            RegisterClassW(&wc);
            let hwnd = CreateWindowExW(0, name.as_ptr(), name.as_ptr(), 0, 0, 0, 0, 0, HWND_MESSAGE, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null());
            if hwnd.is_null() || AddClipboardFormatListener(hwnd) == 0 {
                return;
            }
            let mut last = String::new();
            let mut msg: MSG = std::mem::zeroed();
            while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
                if msg.message == WM_CLIPBOARDUPDATE {
                    if let Some(t) = read() {
                        if t != last {
                            last = t.clone();
                            on_text(t);
                        }
                    }
                } else {
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
        })
        .ok();
}

#[cfg(not(windows))]
pub fn watch(_on_text: impl Fn(String) + Send + 'static) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sorts_clipboard_text() {
        assert_eq!(classify("hello there"), None);
        assert_eq!(classify("https://a.com/x.zip"), Some(Catch::Link("https://a.com/x.zip".into())));
        assert!(matches!(classify("magnet:?xt=urn:btih:abc"), Some(Catch::Magnet(_))));
        assert_eq!(classify("see https://a.com/1.zip and https://a.com/2.pdf"), Some(Catch::Many(2)));
        assert_eq!(classify("see https://a.com/1.zip only"), None);
        assert!(matches!(classify("curl 'https://a.com/f' -H 'cookie: a=b'"), Some(Catch::Curl(_))));
        assert!(video_host("https://www.youtube.com/watch?v=x"));
        assert!(!video_host("https://news.site.com/story"));
    }
}
