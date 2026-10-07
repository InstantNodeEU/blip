use std::fmt;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq)]
pub enum Target {
    Http {
        url: String,
        expect: Option<u16>,
        keyword: Option<String>,
    },
    Tcp {
        addr: String,
    },
    Ping {
        host: String,
    },
    Dns {
        host: String,
    },
}

#[derive(Debug, Clone)]
pub struct Check {
    pub name: String,
    pub group: String,
    pub target: Target,
    pub interval: Duration,
    pub retry_interval: Duration,
    pub timeout: Duration,
    pub retries: u32,
}

#[derive(Debug, Default)]
pub struct Notify {
    pub webhook: Option<String>,
    pub discord: Option<String>,
    pub telegram: Option<(String, String)>,
}

#[derive(Debug)]
pub struct Config {
    pub listen: String,
    pub title: String,
    pub db: String,
    pub keep_days: u32,
    pub notify: Notify,
    pub checks: Vec<Check>,
}

#[derive(Debug, PartialEq)]
pub struct Error {
    pub line: usize,
    pub msg: String,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        if self.line == 0 {
            f.write_str(&self.msg)
        } else {
            write!(f, "line {}: {}", self.line, self.msg)
        }
    }
}

fn err<T>(line: usize, msg: impl Into<String>) -> Result<T, Error> {
    Err(Error {
        line,
        msg: msg.into(),
    })
}

enum Kind<'a> {
    Global,
    Notify,
    Check(&'a str),
}

struct Section<'a> {
    line: usize,
    kind: Kind<'a>,
    entries: Vec<(usize, &'a str, &'a str)>,
}

pub fn load(path: &str) -> Result<Config, Error> {
    let src = std::fs::read_to_string(path).or_else(|e| err(0, e.to_string()))?;
    parse(&src)
}

pub fn parse(src: &str) -> Result<Config, Error> {
    let mut sections = vec![Section {
        line: 0,
        kind: Kind::Global,
        entries: Vec::new(),
    }];

    for (i, raw) in src.lines().enumerate() {
        let n = i + 1;
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(head) = line.strip_prefix('[') {
            let Some(head) = head.strip_suffix(']') else {
                return err(n, "missing ']'");
            };
            let head = head.trim();
            let kind = if head == "notify" {
                Kind::Notify
            } else if let Some(name) = head.strip_prefix("check") {
                let name = unquote(name.trim());
                if name.is_empty() {
                    return err(n, "check needs a name, like [check \"website\"]");
                }
                Kind::Check(name)
            } else {
                return err(n, format!("unknown section [{head}]"));
            };
            sections.push(Section {
                line: n,
                kind,
                entries: Vec::new(),
            });
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return err(n, "expected key = value");
        };
        let section = sections.last_mut().unwrap();
        section.entries.push((n, key.trim(), unquote(value.trim())));
    }

    let mut cfg = Config {
        listen: "127.0.0.1:8080".into(),
        title: "Status".into(),
        db: "blip.db".into(),
        keep_days: 90,
        notify: Notify::default(),
        checks: Vec::new(),
    };

    for s in &sections {
        match s.kind {
            Kind::Global => global(&mut cfg, s)?,
            Kind::Notify => notify(&mut cfg.notify, s)?,
            Kind::Check(name) => {
                if cfg.checks.iter().any(|c| c.name == name) {
                    return err(s.line, format!("duplicate check name \"{name}\""));
                }
                cfg.checks.push(check(name, s)?);
            }
        }
    }

    if cfg.checks.is_empty() {
        return err(0, "no checks configured");
    }
    Ok(cfg)
}

fn global(cfg: &mut Config, s: &Section) -> Result<(), Error> {
    for &(n, key, value) in &s.entries {
        match key {
            "listen" => cfg.listen = value.into(),
            "title" => cfg.title = value.into(),
            "db" => cfg.db = value.into(),
            "keep_days" => {
                cfg.keep_days = number(n, value)?;
                if cfg.keep_days == 0 {
                    return err(n, "keep_days must be at least 1");
                }
            }
            _ => return err(n, format!("unknown key \"{key}\"")),
        }
    }
    Ok(())
}

fn notify(out: &mut Notify, s: &Section) -> Result<(), Error> {
    let mut token = None;
    let mut chat = None;
    for &(n, key, value) in &s.entries {
        match key {
            "webhook" => out.webhook = Some(value.into()),
            "discord" => out.discord = Some(value.into()),
            "telegram_token" => token = Some(value.to_string()),
            "telegram_chat" => chat = Some(value.to_string()),
            _ => return err(n, format!("unknown key \"{key}\"")),
        }
    }
    out.telegram = match (token, chat) {
        (Some(t), Some(c)) => Some((t, c)),
        (None, None) => None,
        _ => {
            return err(
                s.line,
                "telegram needs both telegram_token and telegram_chat",
            )
        }
    };
    Ok(())
}

fn check(name: &str, s: &Section) -> Result<Check, Error> {
    let mut url = None;
    let mut expect = None;
    let mut keyword = None;
    let mut group = String::new();
    let mut interval = Duration::from_secs(60);
    let mut retry_interval = None;
    let mut timeout = Duration::from_secs(10);
    let mut retries = 2;

    for &(n, key, value) in &s.entries {
        match key {
            "url" => url = Some((n, value)),
            "expect" => expect = Some(number(n, value)?),
            "keyword" => keyword = Some(value.to_string()),
            "group" => group = value.into(),
            "interval" => interval = duration(n, value)?,
            "retry_interval" => retry_interval = Some(duration(n, value)?),
            "timeout" => timeout = duration(n, value)?,
            "retries" => retries = number(n, value)?,
            _ => return err(n, format!("unknown key \"{key}\"")),
        }
    }

    let Some((n, url)) = url else {
        return err(s.line, format!("check \"{name}\" has no url"));
    };
    let target = target(n, url, expect, keyword)?;

    Ok(Check {
        name: name.into(),
        group,
        target,
        interval,
        retry_interval: retry_interval.unwrap_or(interval.min(Duration::from_secs(20))),
        timeout,
        retries,
    })
}

fn target(
    n: usize,
    url: &str,
    expect: Option<u16>,
    keyword: Option<String>,
) -> Result<Target, Error> {
    if url.starts_with("http://") || url.starts_with("https://") {
        return Ok(Target::Http {
            url: url.into(),
            expect,
            keyword,
        });
    }
    if expect.is_some() || keyword.is_some() {
        return err(n, "expect and keyword only work with http(s) checks");
    }
    let Some((scheme, rest)) = url.split_once("://") else {
        return err(n, format!("can't tell what kind of check \"{url}\" is"));
    };
    let rest = rest.trim_end_matches('/');
    if rest.is_empty() {
        return err(n, "missing host");
    }
    match scheme {
        "tcp" => {
            let port = rest.rsplit_once(':').map(|(_, p)| p.parse::<u16>());
            match port {
                Some(Ok(p)) if p > 0 => Ok(Target::Tcp { addr: rest.into() }),
                _ => err(n, "tcp checks need a port, like tcp://example.com:22"),
            }
        }
        "ping" | "icmp" => Ok(Target::Ping { host: rest.into() }),
        "dns" => Ok(Target::Dns { host: rest.into() }),
        _ => err(n, format!("unsupported scheme \"{scheme}\"")),
    }
}

fn unquote(s: &str) -> &str {
    s.strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(s)
}

fn number<T: std::str::FromStr>(n: usize, v: &str) -> Result<T, Error> {
    v.parse()
        .or_else(|_| err(n, format!("\"{v}\" is not a valid number")))
}

pub fn duration(n: usize, v: &str) -> Result<Duration, Error> {
    let (num, unit) = match v.find(|c: char| !c.is_ascii_digit()) {
        Some(i) => v.split_at(i),
        None => (v, "s"),
    };
    let num: u64 = number(n, num)?;
    let secs = match unit.trim() {
        "s" => num,
        "m" => num * 60,
        "h" => num * 3600,
        _ => return err(n, format!("bad duration \"{v}\", use 30s, 5m or 1h")),
    };
    if secs == 0 {
        return err(n, "duration must be more than zero");
    }
    Ok(Duration::from_secs(secs))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
# comment
title = "Example Status"
listen = 0.0.0.0:9000

[notify]
discord = https://discord.com/api/webhooks/1/abc
telegram_token = 123:abc
telegram_chat = -100

[check "Website"]
url = https://example.com/
keyword = Example Domain
group = Web
interval = 30s

[check "SSH"]
url = tcp://example.com:22
retries = 0
timeout = 3

[check Router]
url = ping://192.168.1.1
interval = 2m
"#;

    #[test]
    fn parses_sample() {
        let cfg = parse(SAMPLE).unwrap();
        assert_eq!(cfg.title, "Example Status");
        assert_eq!(cfg.listen, "0.0.0.0:9000");
        assert_eq!(cfg.db, "blip.db");
        assert_eq!(cfg.checks.len(), 3);
        assert_eq!(
            cfg.notify.telegram,
            Some(("123:abc".to_string(), "-100".to_string()))
        );

        let web = &cfg.checks[0];
        assert_eq!(web.group, "Web");
        assert_eq!(web.interval, Duration::from_secs(30));
        assert_eq!(web.retry_interval, Duration::from_secs(20));
        assert_eq!(web.retries, 2);
        assert_eq!(
            web.target,
            Target::Http {
                url: "https://example.com/".into(),
                expect: None,
                keyword: Some("Example Domain".into()),
            }
        );

        let ssh = &cfg.checks[1];
        assert_eq!(ssh.retries, 0);
        assert_eq!(ssh.timeout, Duration::from_secs(3));
        assert_eq!(
            ssh.target,
            Target::Tcp {
                addr: "example.com:22".into()
            }
        );

        assert_eq!(cfg.checks[2].name, "Router");
        assert_eq!(cfg.checks[2].interval, Duration::from_secs(120));
    }

    #[test]
    fn rejects_bad_input() {
        let line = |src: &str| parse(src).unwrap_err().line;

        assert_eq!(line("[check \"a\"]\nurl = tcp://host\n"), 2);
        assert_eq!(line("[check \"a\"]\nurl = ftp://host\n"), 2);
        assert_eq!(line("[check \"a\"]\nurl = tcp://h:1\nkeyword = x\n"), 2);
        assert_eq!(line("[check \"a\"]\nurl = https://x\ninterval = 5d\n"), 3);
        assert_eq!(line("[check \"a\"]\nurl = https://x\ncolour = red\n"), 3);
        assert_eq!(
            line("[check \"a\"]\nurl = https://x\n[check \"a\"]\nurl = https://y\n"),
            3
        );
        assert_eq!(line("[check]\nurl = https://x\n"), 1);
        assert_eq!(line("[checks \"a\"\n"), 1);
        assert_eq!(
            line("[notify]\ntelegram_chat = 1\n[check \"a\"]\nurl = https://x\n"),
            1
        );
        assert_eq!(
            parse("title = x\n").unwrap_err().msg,
            "no checks configured"
        );
    }

    #[test]
    fn durations() {
        assert_eq!(duration(1, "45").unwrap(), Duration::from_secs(45));
        assert_eq!(duration(1, "45s").unwrap(), Duration::from_secs(45));
        assert_eq!(duration(1, "5m").unwrap(), Duration::from_secs(300));
        assert_eq!(duration(1, "1h").unwrap(), Duration::from_secs(3600));
        assert!(duration(1, "0").is_err());
        assert!(duration(1, "m").is_err());
        assert!(duration(1, "-5").is_err());
    }
}
