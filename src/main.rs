mod config;
mod notify;
mod probe;
mod state;
mod store;
mod text;
mod web;

use std::net::TcpListener;
use std::process::exit;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use config::Config;
use state::{Change, Status, Tracker};
use store::Store;

const VERSION: &str = env!("CARGO_PKG_VERSION");

const USAGE: &str = "usage: blip [-t] [config]

  config   path to the config file (default: blip.conf)
  -t       check the config and exit
  -V       print the version";

pub struct App {
    pub config: Config,
    pub store: Mutex<Store>,
    pub ids: Vec<i64>,
    pub live: Mutex<Vec<Live>>,
    pub agent: ureq::Agent,
    pub stop: Stop,
}

#[derive(Clone)]
pub struct Live {
    pub status: Status,
    pub ms: u32,
    pub checked: u64,
    pub detail: String,
}

#[derive(Default)]
pub struct Stop {
    flag: Mutex<bool>,
    cv: Condvar,
}

impl Stop {
    /// Sleeps for `d`, returns true if we got asked to stop meanwhile.
    fn sleep(&self, d: Duration) -> bool {
        let flag = self.flag.lock().unwrap();
        let (flag, _) = self.cv.wait_timeout_while(flag, d, |f| !*f).unwrap();
        *flag
    }

    fn set(&self) {
        *self.flag.lock().unwrap() = true;
        self.cv.notify_all();
    }
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn main() {
    let mut path = "blip.conf".to_string();
    let mut test = false;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "-t" => test = true,
            "-V" | "--version" => {
                println!("blip {VERSION}");
                return;
            }
            "-h" | "--help" => {
                println!("{USAGE}");
                return;
            }
            a if a.starts_with('-') => {
                eprintln!("{USAGE}");
                exit(2);
            }
            a => path = a.to_string(),
        }
    }

    let config = config::load(&path).unwrap_or_else(|e| {
        eprintln!("blip: {path}: {e}");
        exit(1);
    });
    if test {
        println!("{path}: ok, {} checks", config.checks.len());
        return;
    }

    if let Err(e) = run(config) {
        eprintln!("blip: {e}");
        exit(1);
    }
}

fn run(config: Config) -> Result<(), String> {
    #[cfg(target_env = "gnu")]
    unsafe {
        // glibc gives every thread its own malloc arena, which adds up fast
        // with one thread per check. We allocate almost nothing, so share one.
        libc::mallopt(libc::M_ARENA_MAX, 1);
    }

    let signals = block_signals();

    let store = Store::open(&config.db).map_err(|e| format!("{}: {e}", config.db))?;
    let ids = config
        .checks
        .iter()
        .map(|c| store.monitor_id(&c.name))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    let _ = store.prune(now(), config.keep_days);

    let listener =
        TcpListener::bind(&config.listen).map_err(|e| format!("{}: {e}", config.listen))?;

    let live = vec![
        Live {
            status: Status::Pending,
            ms: 0,
            checked: 0,
            detail: String::new(),
        };
        config.checks.len()
    ];
    let agent = ureq::AgentBuilder::new()
        .max_idle_connections(0)
        .user_agent(concat!("blip/", env!("CARGO_PKG_VERSION")))
        .build();

    eprintln!(
        "blip {VERSION}: {} checks, status page on http://{}",
        config.checks.len(),
        config.listen
    );

    let app = Arc::new(App {
        config,
        store: Mutex::new(store),
        ids,
        live: Mutex::new(live),
        agent,
        stop: Stop::default(),
    });

    for i in 0..app.config.checks.len() {
        let app = app.clone();
        spawn(&format!("check-{i}"), move || watch(&app, i));
    }
    {
        let app = app.clone();
        spawn("prune", move || {
            while !app.stop.sleep(Duration::from_secs(3600)) {
                let _ = app.store.lock().unwrap().prune(now(), app.config.keep_days);
            }
        });
    }
    {
        let app = app.clone();
        spawn("http", move || web::serve(listener, &app));
    }

    wait_for_signal(&signals);
    eprintln!("blip: shutting down");
    app.stop.set();
    app.store.lock().unwrap().close();
    exit(0);
}

fn spawn(name: &str, f: impl FnOnce() + Send + 'static) {
    thread::Builder::new()
        .name(name.into())
        .stack_size(512 * 1024)
        .spawn(f)
        .expect("spawning thread");
}

fn watch(app: &App, i: usize) {
    let check = &app.config.checks[i];
    let id = app.ids[i];
    let mut tracker = Tracker::new(check.retries);

    // spread the first round out a little instead of firing everything at once
    if app.stop.sleep(Duration::from_millis(150 * i as u64)) {
        return;
    }

    loop {
        let out = probe::run(&app.agent, &check.target, check.timeout);
        let ts = now();
        let change = tracker.record(out.ok, ts);
        let status = tracker.status();

        // Failed attempts that are still being retried don't count against
        // uptime. Only confirmed states do.
        if !tracker.retrying() {
            let store = app.store.lock().unwrap();
            if let Err(e) = store.record(id, ts, status == Status::Up, out.ms) {
                eprintln!("blip: saving result for {}: {e}", check.name);
            }
        }

        if let Some(change) = change {
            let (up, detail, msg) = match change {
                Change::Down => {
                    let msg = format!("{} is down: {}", check.name, out.detail);
                    (false, out.detail.clone(), msg)
                }
                Change::Up { downtime } => {
                    let after = format!("after {}", text::span(downtime));
                    let msg = format!("{} is back up {after}", check.name);
                    (true, after, msg)
                }
            };
            eprintln!("blip: {msg}");
            let _ = app.store.lock().unwrap().event(id, ts, up, &detail);
            notify::send(&app.agent, &app.config.notify, check, up, &msg, ts);
        }

        app.live.lock().unwrap()[i] = Live {
            status,
            ms: out.ms,
            checked: ts,
            detail: out.detail,
        };

        let wait = if tracker.retrying() {
            check.retry_interval
        } else {
            check.interval
        };
        if app.stop.sleep(wait) {
            return;
        }
    }
}

// SIGTERM and SIGINT are blocked in every thread and picked up with sigwait
// in main, so shutdown runs as normal code instead of inside a handler.
fn block_signals() -> libc::sigset_t {
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGTERM);
        libc::sigaddset(&mut set, libc::SIGINT);
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
        set
    }
}

fn wait_for_signal(set: &libc::sigset_t) {
    let mut sig = 0;
    unsafe {
        libc::sigwait(set, &mut sig);
    }
}
