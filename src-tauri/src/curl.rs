use base64::Engine;

#[derive(Debug, Default, PartialEq)]
pub struct Request {
    pub url: String,
    pub headers: Vec<(String, String)>,
}

struct Words {
    out: Vec<String>,
    cur: String,
    started: bool,
}

impl Words {
    fn new() -> Self {
        Self { out: Vec::new(), cur: String::new(), started: false }
    }
    fn push(&mut self, c: char) {
        self.cur.push(c);
        self.started = true;
    }
    fn split(&mut self) {
        if self.started {
            self.out.push(std::mem::take(&mut self.cur));
            self.started = false;
        }
    }
    fn done(mut self) -> Vec<String> {
        self.split();
        self.out
    }
}

fn words_cmd(cmd: &str) -> Vec<String> {
    let mut w = Words::new();
    let mut quoted = false;
    let mut chars = cmd.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '^' => match chars.next() {
                Some('"') => {
                    quoted = !quoted;
                    w.started = true;
                }
                Some('\r') | Some('\n') => {
                    if chars.peek() == Some(&'\n') {
                        chars.next();
                    }
                }
                Some('\\') if chars.peek() == Some(&'^') => {
                    chars.next();
                    if let Some(n) = chars.next() {
                        w.push(n);
                    }
                }
                Some(o) => w.push(o),
                None => {}
            },
            '\\' if chars.peek() == Some(&'^') => {
                chars.next();
                if let Some(n) = chars.next() {
                    w.push(n);
                }
            }
            '"' => {
                quoted = !quoted;
                w.started = true;
            }
            c if c.is_whitespace() && !quoted => w.split(),
            c => w.push(c),
        }
    }
    w.done()
}

fn words_sh(cmd: &str) -> Vec<String> {
    let mut w = Words::new();
    let mut chars = cmd.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some('\r') | Some('\n') => {
                    if chars.peek() == Some(&'\n') {
                        chars.next();
                    }
                }
                Some(n) => w.push(n),
                None => {}
            },
            '\'' => {
                w.started = true;
                for n in chars.by_ref() {
                    if n == '\'' {
                        break;
                    }
                    w.cur.push(n);
                }
            }
            '$' if chars.peek() == Some(&'\'') => {
                chars.next();
                w.started = true;
                while let Some(n) = chars.next() {
                    match n {
                        '\'' => break,
                        '\\' => match chars.next() {
                            Some('n') => w.cur.push('\n'),
                            Some('t') => w.cur.push('\t'),
                            Some(o) => w.cur.push(o),
                            None => {}
                        },
                        o => w.cur.push(o),
                    }
                }
            }
            '"' => {
                w.started = true;
                while let Some(n) = chars.next() {
                    match n {
                        '"' => break,
                        '\\' if matches!(chars.peek(), Some('"' | '\\' | '$' | '`')) => {
                            if let Some(o) = chars.next() {
                                w.cur.push(o);
                            }
                        }
                        o => w.cur.push(o),
                    }
                }
            }
            c if c.is_whitespace() => w.split(),
            c => w.push(c),
        }
    }
    w.done()
}

pub fn parse(cmd: &str) -> Option<Request> {
    let cmd = cmd.trim();
    let w = if cmd.contains("^\"") || cmd.contains("^\n") || cmd.contains("^\r\n") { words_cmd(cmd) } else { words_sh(cmd) };
    let first = w.first()?.to_ascii_lowercase();
    if first != "curl" && first != "curl.exe" {
        return None;
    }
    let mut r = Request::default();
    let mut i = 1;
    while i < w.len() {
        let a = w[i].as_str();
        let next = w.get(i + 1).cloned().unwrap_or_default();
        let mut used = true;
        match a {
            "-H" | "--header" => {
                if let Some((k, v)) = next.split_once(':') {
                    r.headers.push((k.trim().to_string(), v.trim().to_string()));
                }
            }
            "-b" | "--cookie" => r.headers.push(("Cookie".into(), next)),
            "-A" | "--user-agent" => r.headers.push(("User-Agent".into(), next)),
            "-e" | "--referer" => r.headers.push(("Referer".into(), next)),
            "-u" | "--user" => r.headers.push(("Authorization".into(), format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(next)))),
            "--url" => r.url = next,
            "-X" | "--request" | "-d" | "--data" | "--data-raw" | "--data-binary" | "-o" | "--output" | "-x" | "--proxy" | "-m" | "--max-time" | "--connect-timeout" => {}
            s => {
                used = false;
                if !s.starts_with('-') && r.url.is_empty() {
                    r.url = s.to_string();
                }
            }
        }
        i += if used { 2 } else { 1 };
    }
    (r.url.starts_with("http://") || r.url.starts_with("https://")).then_some(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bash_style() {
        let c = "curl 'https://x.com/f.zip?a=1' \\\n  -H 'accept: */*' \\\n  -b 'sid=abc; t=1' \\\n  -H $'referer: https://x.com/page' \\\n  --compressed";
        let r = parse(c).unwrap();
        assert_eq!(r.url, "https://x.com/f.zip?a=1");
        assert!(r.headers.contains(&("Cookie".into(), "sid=abc; t=1".into())));
        assert!(r.headers.contains(&("referer".into(), "https://x.com/page".into())));
    }

    #[test]
    fn cmd_style() {
        let c = "curl ^\"https://x.com/f.zip^\" ^\n  -H ^\"cookie: a=b^\" ^\n  -H ^\"user-agent: Mozilla 5^\" ^\n  -H ^\"x-json: ^\\^\"q^\\^\"^\"";
        let r = parse(c).unwrap();
        assert_eq!(r.url, "https://x.com/f.zip");
        assert_eq!(r.headers[0], ("cookie".into(), "a=b".into()));
        assert_eq!(r.headers[1], ("user-agent".into(), "Mozilla 5".into()));
        assert_eq!(r.headers[2].1, "\"q\"");
    }

    #[test]
    fn not_curl() {
        assert!(parse("wget https://x.com").is_none());
        assert!(parse("curl -X POST").is_none());
    }
}
