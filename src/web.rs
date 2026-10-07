use std::fmt::Write as _;
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

use crate::state::Status;
use crate::store::Event;
use crate::text::{self, html, json};
use crate::{now, App, Live};

const PAGE: &str = include_str!("page.html");
const DAYS: u64 = 90;

pub fn serve(listener: TcpListener, app: &App) {
    for conn in listener.incoming().flatten() {
        let _ = handle(conn, app);
    }
}

fn handle(mut conn: TcpStream, app: &App) -> io::Result<()> {
    conn.set_read_timeout(Some(Duration::from_secs(3)))?;
    conn.set_write_timeout(Some(Duration::from_secs(5)))?;

    let mut buf = [0u8; 4096];
    let mut len = 0;
    while !buf[..len].windows(4).any(|w| w == b"\r\n\r\n") {
        if len == buf.len() {
            return reply(
                &mut conn,
                "431 Request Header Fields Too Large",
                "text/plain",
                "",
                false,
            );
        }
        let n = conn.read(&mut buf[len..])?;
        if n == 0 {
            return Ok(());
        }
        len += n;
    }

    let head = String::from_utf8_lossy(&buf[..len]);
    let mut parts = head.split(' ');
    let method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("/");
    let path = path.split('?').next().unwrap_or(path);

    if method != "GET" && method != "HEAD" {
        return reply(&mut conn, "405 Method Not Allowed", "text/plain", "", false);
    }
    let head_only = method == "HEAD";

    match path {
        "/" => {
            let body = page(app);
            reply(
                &mut conn,
                "200 OK",
                "text/html; charset=utf-8",
                &body,
                head_only,
            )
        }
        "/api/status" => {
            let body = api(app);
            reply(&mut conn, "200 OK", "application/json", &body, head_only)
        }
        "/healthz" => reply(&mut conn, "200 OK", "text/plain", "ok\n", head_only),
        _ => reply(
            &mut conn,
            "404 Not Found",
            "text/plain",
            "not found\n",
            head_only,
        ),
    }
}

fn reply(
    conn: &mut TcpStream,
    status: &str,
    ctype: &str,
    body: &str,
    head_only: bool,
) -> io::Result<()> {
    let header = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\n\
         Cache-Control: no-cache\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n",
        body.len()
    );
    conn.write_all(header.as_bytes())?;
    if !head_only {
        conn.write_all(body.as_bytes())?;
    }
    conn.flush()
}

struct Row {
    live: Live,
    days: Vec<(u64, u64, u64)>,
    day: (u64, u64),
    total: (u64, u64),
}

fn snapshot(app: &App, with_days: bool) -> (Vec<Row>, Vec<Event>) {
    let today = now() / 86400;
    let live = app.live.lock().unwrap().clone();
    let store = app.store.lock().unwrap();

    let rows = live
        .into_iter()
        .zip(&app.ids)
        .map(|(live, &id)| {
            let days = store.days(id, today + 1 - DAYS).unwrap_or_default();
            let total = days
                .iter()
                .fold((0, 0), |(u, t), &(_, up, total)| (u + up, t + total));
            let (up, all, _) = store.since(id, now() / 3600 - 23).unwrap_or_default();
            Row {
                live,
                days: if with_days { days } else { Vec::new() },
                day: (up, all),
                total,
            }
        })
        .collect();

    let events = if with_days {
        store.events(&app.ids, 10).unwrap_or_default()
    } else {
        Vec::new()
    };
    (rows, events)
}

fn overall(rows: &[Row]) -> (Status, usize) {
    let down = rows
        .iter()
        .filter(|r| r.live.status == Status::Down)
        .count();
    if down > 0 {
        (Status::Down, down)
    } else if rows.iter().any(|r| r.live.status == Status::Pending) {
        (Status::Pending, 0)
    } else {
        (Status::Up, 0)
    }
}

fn pct((up, total): (u64, u64)) -> Option<String> {
    if total == 0 {
        return None;
    }
    let bp = up * 10_000 / total;
    Some(if bp == 10_000 {
        "100%".into()
    } else {
        format!("{}.{:02}%", bp / 100, bp % 100)
    })
}

fn page(app: &App) -> String {
    let cfg = &app.config;
    let (rows, events) = snapshot(app, true);
    let today = now() / 86400;
    let mut b = String::with_capacity(16 * 1024 + rows.len() * 4096);

    let (status, down) = overall(&rows);
    let summary = match status {
        Status::Up => "All systems operational".to_string(),
        Status::Pending => "Checking...".to_string(),
        Status::Down if down == rows.len() => "Everything is down".to_string(),
        Status::Down => format!("{down} of {} checks are down", rows.len()),
    };
    let _ = write!(
        b,
        "<header><h1>{}</h1><p class=\"summary\"><span class=\"dot {}\"></span>{summary}</p>\
         <p class=\"hint\">Uptime over the last {DAYS} days, today on the right.</p></header>",
        html(&cfg.title),
        status.as_str(),
    );

    let mut groups: Vec<&str> = Vec::new();
    for c in &cfg.checks {
        if !groups.contains(&c.group.as_str()) {
            groups.push(&c.group);
        }
    }

    for group in groups {
        b.push_str("<section>");
        if !group.is_empty() {
            let _ = write!(b, "<h2>{}</h2>", html(group));
        }
        for (check, row) in cfg.checks.iter().zip(&rows) {
            if check.group != group {
                continue;
            }
            let live = &row.live;
            let mut meta = pct(row.total).unwrap_or_default();
            if live.status == Status::Up {
                let _ = write!(meta, " &middot; {} ms", live.ms);
            }
            let tip = match live.status {
                Status::Down => html(&live.detail),
                s => s.as_str().to_string(),
            };
            let _ = write!(
                b,
                "<div class=\"check\"><div class=\"row\"><span class=\"name\" title=\"{tip}\">\
                 <span class=\"dot {}\"></span>{}</span><span class=\"meta\">{meta}</span></div>\
                 <div class=\"bars\">",
                live.status.as_str(),
                html(&check.name),
            );
            let mut days = row.days.iter().peekable();
            for d in today + 1 - DAYS..=today {
                let Some(&(_, up, total)) = days.next_if(|x| x.0 == d) else {
                    let _ = write!(b, "<i title=\"{}: no data\"></i>", text::day(d));
                    continue;
                };
                let class = match up * 1000 / total {
                    1000 => "u",
                    950.. => "w",
                    _ => "d",
                };
                let p = pct((up, total)).unwrap_or_default();
                let _ = write!(
                    b,
                    "<i class=\"{class}\" title=\"{}: {p}\"></i>",
                    text::day(d)
                );
            }
            b.push_str("</div></div>");
        }
        b.push_str("</section>");
    }

    if !events.is_empty() {
        b.push_str("<section class=\"events\"><h2>Recent events</h2><ul>");
        for e in &events {
            let what = if e.up {
                format!("{} is back up {}", html(&e.name), html(&e.detail))
            } else {
                format!("{} went down: {}", html(&e.name), html(&e.detail))
            };
            let _ = write!(
                b,
                "<li><time>{}</time><span>{what}</span></li>",
                text::timestamp(e.ts)
            );
        }
        b.push_str("</ul></section>");
    }

    let _ = write!(
        b,
        "<footer>Updated {}. Powered by <a href=\"https://github.com/instantnode/blip\">blip</a>.</footer>",
        text::timestamp(now())
    );

    PAGE.replacen("<!--title-->", &html(&cfg.title), 1)
        .replacen("<!--body-->", &b, 1)
}

fn api(app: &App) -> String {
    let (rows, _) = snapshot(app, false);
    let opt = |v: Option<String>| v.unwrap_or_else(|| "null".into());
    let ratio = |(up, total): (u64, u64)| {
        (total > 0).then(|| format!("{:.3}", up as f64 * 100.0 / total as f64))
    };

    let mut b = String::new();
    let _ = write!(
        b,
        r#"{{"title":{},"status":"{}","checks":["#,
        json(&app.config.title),
        overall(&rows).0.as_str()
    );
    for (i, (check, row)) in app.config.checks.iter().zip(&rows).enumerate() {
        let live = &row.live;
        let checked = live.checked > 0;
        let _ = write!(
            b,
            r#"{}{{"name":{},"group":{},"status":"{}","latency_ms":{},"last_check":{},"uptime_24h":{},"uptime_90d":{},"error":{}}}"#,
            if i > 0 { "," } else { "" },
            json(&check.name),
            json(&check.group),
            live.status.as_str(),
            opt((checked && live.status == Status::Up).then(|| live.ms.to_string())),
            opt(checked.then(|| live.checked.to_string())),
            opt(ratio(row.day)),
            opt(ratio(row.total)),
            opt((live.status == Status::Down).then(|| json(&live.detail))),
        );
    }
    b.push_str("]}");
    b
}

#[cfg(test)]
mod tests {
    use super::pct;

    #[test]
    fn percentages() {
        assert_eq!(pct((0, 0)), None);
        assert_eq!(pct((10, 10)).unwrap(), "100%");
        assert_eq!(pct((9999, 10000)).unwrap(), "99.99%");
        assert_eq!(pct((99999, 100000)).unwrap(), "99.99%");
        assert_eq!(pct((1, 3)).unwrap(), "33.33%");
    }
}
