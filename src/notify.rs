use std::time::Duration;

use crate::config::{Check, Notify};
use crate::text::json;

pub fn send(agent: &ureq::Agent, to: &Notify, check: &Check, up: bool, message: &str, ts: u64) {
    if let Some(url) = &to.webhook {
        let body = format!(
            r#"{{"check":{},"group":{},"status":"{}","message":{},"time":{ts}}}"#,
            json(&check.name),
            json(&check.group),
            if up { "up" } else { "down" },
            json(message),
        );
        post(agent, "webhook", url, &body);
    }
    if let Some(url) = &to.discord {
        post(
            agent,
            "discord",
            url,
            &format!(r#"{{"content":{}}}"#, json(message)),
        );
    }
    if let Some((token, chat)) = &to.telegram {
        let url = format!("https://api.telegram.org/bot{token}/sendMessage");
        let body = format!(r#"{{"chat_id":{},"text":{}}}"#, json(chat), json(message));
        post(agent, "telegram", &url, &body);
    }
}

fn post(agent: &ureq::Agent, what: &str, url: &str, body: &str) {
    let res = agent
        .post(url)
        .timeout(Duration::from_secs(10))
        .set("Content-Type", "application/json")
        .send_string(body);
    if let Err(e) = res {
        let reason = match e {
            ureq::Error::Status(code, _) => format!("HTTP {code}"),
            ureq::Error::Transport(t) => t.kind().to_string(),
        };
        eprintln!("notify: {what} failed: {reason}");
    }
}
