//! sysTUI — a small system monitor for the terminal.
//!
//! CPU per core, memory, network, disk and a sortable process table, read
//! straight from `/proc`. See the README for the keys.

mod metrics;
mod procfs;
mod state;
mod ui;

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::Path;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{self, Event, KeyEventKind};

use metrics::{Metrics, compute};
use procfs::{Snapshot, parse_passwd, read_snapshot};
use state::App;

const PROC: &str = "/proc";

fn hostname() -> String {
    fs::read_to_string("/etc/hostname")
        .map(|s| s.trim().to_string())
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "localhost".into())
}

/// Two readings `gap` apart, so the very first frame already has real rates.
fn first_metrics(users: &HashMap<u32, String>, gap: Duration) -> (Snapshot, Metrics) {
    let a = read_snapshot(Path::new(PROC));
    std::thread::sleep(gap);
    let b = read_snapshot(Path::new(PROC));
    let m = compute(&a, &b, users);
    (b, m)
}

fn users() -> HashMap<u32, String> {
    parse_passwd(&fs::read_to_string("/etc/passwd").unwrap_or_default())
}

/// `systui --snapshot [WxH]`: print one frame as text and exit.
fn snapshot(arg: Option<String>) {
    let (w, h) = arg
        .as_deref()
        .and_then(|s| s.split_once('x'))
        .and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)))
        .unwrap_or((120, 36));
    let (_, m) = first_metrics(&users(), Duration::from_millis(500));
    let mut app = App::new(hostname());
    app.update(m);
    println!("{}", ui::render_text(&mut app, w, h));
}

fn main() -> io::Result<()> {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("--snapshot") => {
            snapshot(args.next());
            return Ok(());
        }
        Some("-h") | Some("--help") => {
            println!(
                "sysTUI: monitor de sistema para el terminal\n\nuso: systui [--snapshot [ANCHOxALTO]]\n\n  --snapshot  imprime un fotograma como texto y sale\n  ?           dentro de sysTUI, muestra las teclas"
            );
            return Ok(());
        }
        Some(other) => {
            eprintln!("opción desconocida: {other} (prueba --help)");
            std::process::exit(2);
        }
        None => {}
    }

    let users = users();
    let (mut prev, first) = first_metrics(&users, Duration::from_millis(300));
    let mut app = App::new(hostname());
    app.update(first);

    let mut terminal = ratatui::init();
    let result = run(&mut terminal, &mut app, &mut prev, &users);
    ratatui::restore();
    result
}

fn run(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    prev: &mut Snapshot,
    users: &HashMap<u32, String>,
) -> io::Result<()> {
    let mut next_sample = Instant::now() + Duration::from_millis(app.interval_ms);
    while !app.quit {
        terminal.draw(|f| ui::draw(f, app))?;

        let wait = next_sample.saturating_duration_since(Instant::now());
        if event::poll(wait)?
            && let Event::Key(key) = event::read()?
        {
            // Windows-style terminals also report key releases.
            if key.kind != KeyEventKind::Release {
                app.handle_key(key);
            }
        }
        if Instant::now() >= next_sample {
            let cur = read_snapshot(Path::new(PROC));
            app.update(compute(prev, &cur, users));
            *prev = cur;
            next_sample = Instant::now() + Duration::from_millis(app.interval_ms);
        }
    }
    Ok(())
}
