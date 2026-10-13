//! Application state and input handling. No drawing and no `/proc` reads
//! here: it consumes [`Metrics`] and key events, which keeps it testable.

use std::collections::VecDeque;
use std::process::Command;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::metrics::{Metrics, ProcRow};

/// Samples kept per graph; wider terminals just show more of them.
pub const HISTORY: usize = 240;
pub const MIN_INTERVAL_MS: u64 = 250;
pub const MAX_INTERVAL_MS: u64 = 5000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortKey {
    Cpu,
    Mem,
    Pid,
    Name,
}

impl SortKey {
    pub fn label(self) -> &'static str {
        match self {
            SortKey::Cpu => "CPU",
            SortKey::Mem => "MEM",
            SortKey::Pid => "PID",
            SortKey::Name => "NOMBRE",
        }
    }
    /// CPU and memory read best biggest-first; PID and name ascending.
    fn natural_descending(self) -> bool {
        matches!(self, SortKey::Cpu | SortKey::Mem)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Filter,
    Kill { pid: u32, name: String },
    Help,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    Term,
    Kill,
}

/// Fixed-size ring of the latest values of one metric.
#[derive(Debug, Clone, Default)]
pub struct History(VecDeque<f64>);

impl History {
    pub fn push(&mut self, v: f64) {
        if self.0.len() == HISTORY {
            self.0.pop_front();
        }
        self.0.push_back(v);
    }
    /// The newest `n` values, oldest first.
    pub fn last(&self, n: usize) -> Vec<f64> {
        let skip = self.0.len().saturating_sub(n);
        self.0.iter().skip(skip).copied().collect()
    }
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.0.len()
    }
}

pub struct App {
    pub metrics: Metrics,
    pub cpu_hist: History,
    pub mem_hist: History,
    pub net_rx_hist: History,
    pub net_tx_hist: History,
    pub disk_r_hist: History,
    pub disk_w_hist: History,
    pub sort: SortKey,
    pub reverse: bool,
    pub filter: String,
    pub mode: Mode,
    pub selected_pid: Option<u32>,
    pub paused: bool,
    pub interval_ms: u64,
    pub status: Option<String>,
    pub hostname: String,
    pub quit: bool,
    /// Rows the process table can show; set by the UI so paging matches.
    pub page: usize,
}

impl App {
    pub fn new(hostname: String) -> Self {
        App {
            metrics: Metrics::default(),
            cpu_hist: History::default(),
            mem_hist: History::default(),
            net_rx_hist: History::default(),
            net_tx_hist: History::default(),
            disk_r_hist: History::default(),
            disk_w_hist: History::default(),
            sort: SortKey::Cpu,
            reverse: false,
            filter: String::new(),
            mode: Mode::Normal,
            selected_pid: None,
            paused: false,
            interval_ms: 1000,
            status: None,
            hostname,
            quit: false,
            page: 10,
        }
    }

    /// Take a new sample. While paused the screen is frozen, history included.
    pub fn update(&mut self, m: Metrics) {
        if self.paused {
            return;
        }
        self.cpu_hist.push(m.cpu as f64);
        let mem_pct = if m.mem.total_kb > 0 {
            m.mem.used_kb() as f64 * 100.0 / m.mem.total_kb as f64
        } else {
            0.0
        };
        self.mem_hist.push(mem_pct);
        self.net_rx_hist.push(m.net_rx_s);
        self.net_tx_hist.push(m.net_tx_s);
        self.disk_r_hist.push(m.disk_read_s);
        self.disk_w_hist.push(m.disk_write_s);
        self.metrics = m;
    }

    /// Processes after filter and sort. The filter matches name, command,
    /// user or PID, case-insensitively.
    pub fn visible(&self) -> Vec<&ProcRow> {
        let needle = self.filter.to_lowercase();
        let mut rows: Vec<&ProcRow> = self
            .metrics
            .procs
            .iter()
            .filter(|p| {
                needle.is_empty()
                    || p.name.to_lowercase().contains(&needle)
                    || p.cmd.to_lowercase().contains(&needle)
                    || p.user.to_lowercase().contains(&needle)
                    || p.pid.to_string().contains(&needle)
            })
            .collect();
        rows.sort_by(|a, b| {
            let ord = match self.sort {
                SortKey::Cpu => b.cpu.total_cmp(&a.cpu),
                SortKey::Mem => b.rss_kb.cmp(&a.rss_kb),
                SortKey::Pid => a.pid.cmp(&b.pid),
                SortKey::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
            };
            // Ties fall back to PID so the order never flickers between samples.
            ord.then(a.pid.cmp(&b.pid))
        });
        if self.reverse {
            rows.reverse();
        }
        rows
    }

    /// Index of the selected process in [`App::visible`]. The selection is
    /// remembered by PID, so it follows the process when the order changes;
    /// if the process is gone, the first row is selected.
    pub fn selected_index(&self) -> Option<usize> {
        let rows = self.visible();
        if rows.is_empty() {
            return None;
        }
        Some(
            self.selected_pid
                .and_then(|pid| rows.iter().position(|p| p.pid == pid))
                .unwrap_or(0),
        )
    }

    fn select_at(&mut self, index: isize) {
        let pid = {
            let rows = self.visible();
            if rows.is_empty() {
                return;
            }
            rows[index.clamp(0, rows.len() as isize - 1) as usize].pid
        };
        self.selected_pid = Some(pid);
    }

    fn move_selection(&mut self, delta: isize) {
        if let Some(i) = self.selected_index() {
            self.select_at(i as isize + delta);
        }
    }

    fn set_sort(&mut self, key: SortKey) {
        if self.sort == key {
            self.reverse = !self.reverse;
        } else {
            self.sort = key;
            self.reverse = false;
        }
        self.status = Some(format!(
            "orden: {} {}",
            key.label(),
            if self.reverse != key.natural_descending() {
                "↑"
            } else {
                "↓"
            }
        ));
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        match self.mode.clone() {
            Mode::Normal => self.key_normal(key),
            Mode::Filter => self.key_filter(key),
            Mode::Kill { pid, .. } => self.key_kill(key, pid),
            Mode::Help => self.mode = Mode::Normal,
        }
    }

    fn key_normal(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.quit = true;
            return;
        }
        self.status = None;
        match key.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Esc if !self.filter.is_empty() => self.filter.clear(),
            KeyCode::Esc => {}
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Char('x') | KeyCode::Delete => self.ask_kill(),
            KeyCode::PageDown => self.move_selection(self.page as isize),
            KeyCode::PageUp => self.move_selection(-(self.page as isize)),
            KeyCode::Home | KeyCode::Char('g') => self.select_at(0),
            KeyCode::End | KeyCode::Char('G') => self.select_at(isize::MAX / 2),
            KeyCode::Char('c') => self.set_sort(SortKey::Cpu),
            KeyCode::Char('m') => self.set_sort(SortKey::Mem),
            KeyCode::Char('p') => self.set_sort(SortKey::Pid),
            KeyCode::Char('n') => self.set_sort(SortKey::Name),
            KeyCode::Char('r') => self.reverse = !self.reverse,
            KeyCode::Char('/') => self.mode = Mode::Filter,
            KeyCode::Char(' ') => {
                self.paused = !self.paused;
                self.status = Some(if self.paused {
                    "en pausa".into()
                } else {
                    "reanudado".into()
                });
            }
            KeyCode::Char('+') | KeyCode::Char('=') => {
                self.interval_ms = (self.interval_ms / 2).max(MIN_INTERVAL_MS);
            }
            KeyCode::Char('-') => {
                self.interval_ms = (self.interval_ms * 2).min(MAX_INTERVAL_MS);
            }
            KeyCode::Char('?') | KeyCode::Char('h') => self.mode = Mode::Help,
            _ => {}
        }
    }

    fn key_filter(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Enter => self.mode = Mode::Normal,
            KeyCode::Esc => {
                self.filter.clear();
                self.mode = Mode::Normal;
            }
            KeyCode::Backspace => {
                self.filter.pop();
            }
            KeyCode::Char(c) => self.filter.push(c),
            _ => {}
        }
    }

    fn ask_kill(&mut self) {
        let Some(i) = self.selected_index() else {
            return;
        };
        let (pid, name) = {
            let p = self.visible()[i];
            (p.pid, p.name.clone())
        };
        if pid == std::process::id() {
            self.status = Some("no me voy a matar a mí mismo".into());
        } else if pid == 1 {
            self.status = Some("el PID 1 no se toca".into());
        } else {
            self.mode = Mode::Kill { pid, name };
        }
    }

    fn key_kill(&mut self, key: KeyEvent, pid: u32) {
        let signal = match key.code {
            KeyCode::Enter | KeyCode::Char('y') => Some(Signal::Term),
            KeyCode::Char('9') => Some(Signal::Kill),
            _ => None,
        };
        if let Some(signal) = signal {
            self.status = Some(match send_signal(pid, signal) {
                Ok(()) => format!(
                    "enviada {} a {pid}",
                    if signal == Signal::Term {
                        "TERM"
                    } else {
                        "KILL"
                    }
                ),
                Err(e) => format!("no se pudo: {e}"),
            });
        }
        self.mode = Mode::Normal;
    }
}

/// Send a signal with the system `kill`. The error text is the tool's own
/// (typically "Operation not permitted" for another user's process).
pub fn send_signal(pid: u32, signal: Signal) -> Result<(), String> {
    let flag = match signal {
        Signal::Term => "-TERM",
        Signal::Kill => "-KILL",
    };
    let out = Command::new("kill")
        .arg(flag)
        .arg(pid.to_string())
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    fn row(pid: u32, name: &str, cpu: f32, rss_kb: u64) -> ProcRow {
        ProcRow {
            pid,
            user: "adrian".into(),
            state: 'S',
            cpu,
            rss_kb,
            name: name.into(),
            cmd: format!("/usr/bin/{name} --flag"),
        }
    }

    fn app() -> App {
        let mut a = App::new("host".into());
        a.update(Metrics {
            procs: vec![
                row(10, "zsh", 1.0, 5000),
                row(20, "Firefox", 40.0, 900_000),
                row(30, "cargo", 90.0, 200_000),
                row(40, "alacritty", 5.0, 80_000),
            ],
            ..Metrics::default()
        });
        a
    }

    fn pids(a: &App) -> Vec<u32> {
        a.visible().iter().map(|p| p.pid).collect()
    }

    #[test]
    fn default_order_is_cpu_descending() {
        assert_eq!(pids(&app()), vec![30, 20, 40, 10]);
    }

    #[test]
    fn sort_keys_and_reverse() {
        let mut a = app();
        a.handle_key(key(KeyCode::Char('m')));
        assert_eq!(pids(&a), vec![20, 30, 40, 10]);
        a.handle_key(key(KeyCode::Char('m'))); // same key again flips
        assert_eq!(pids(&a), vec![10, 40, 30, 20]);
        a.handle_key(key(KeyCode::Char('n')));
        assert_eq!(pids(&a), vec![40, 30, 20, 10]); // alacritty cargo Firefox zsh
        a.handle_key(key(KeyCode::Char('p')));
        assert_eq!(pids(&a), vec![10, 20, 30, 40]);
        a.handle_key(key(KeyCode::Char('r')));
        assert_eq!(pids(&a), vec![40, 30, 20, 10]);
    }

    #[test]
    fn filter_matches_name_cmd_user_and_pid_ignoring_case() {
        let mut a = app();
        a.handle_key(key(KeyCode::Char('/')));
        for c in "FIRE".chars() {
            a.handle_key(key(KeyCode::Char(c)));
        }
        assert_eq!(pids(&a), vec![20]);
        a.handle_key(key(KeyCode::Enter));
        assert_eq!(a.mode, Mode::Normal);
        assert_eq!(a.filter, "FIRE");
        a.handle_key(key(KeyCode::Esc)); // Esc in normal mode clears it
        assert_eq!(pids(&a).len(), 4);
        a.filter = "--flag".into(); // matches the command line
        assert_eq!(pids(&a).len(), 4);
        a.filter = "30".into(); // matches a pid
        assert_eq!(pids(&a), vec![30]);
        a.filter = "adrian".into(); // matches the user
        assert_eq!(pids(&a).len(), 4);
    }

    #[test]
    fn selection_follows_the_pid_when_the_order_changes() {
        let mut a = app();
        a.handle_key(key(KeyCode::Down)); // second row: pid 20
        assert_eq!(a.selected_pid, Some(20));
        a.update(Metrics {
            procs: vec![row(20, "Firefox", 99.0, 1), row(30, "cargo", 10.0, 1)],
            ..Metrics::default()
        });
        assert_eq!(a.selected_index(), Some(0)); // pid 20 is now first
    }

    #[test]
    fn selection_falls_back_when_the_process_dies() {
        let mut a = app();
        a.selected_pid = Some(999);
        assert_eq!(a.selected_index(), Some(0));
        let empty = App::new("h".into());
        assert_eq!(empty.selected_index(), None);
    }

    #[test]
    fn navigation_is_clamped() {
        let mut a = app();
        a.handle_key(key(KeyCode::Up));
        assert_eq!(a.selected_index(), Some(0));
        a.handle_key(key(KeyCode::End));
        assert_eq!(a.selected_index(), Some(3));
        a.handle_key(key(KeyCode::Down));
        assert_eq!(a.selected_index(), Some(3));
        a.handle_key(key(KeyCode::PageUp));
        assert_eq!(a.selected_index(), Some(0));
        a.handle_key(key(KeyCode::Char('G')));
        a.handle_key(key(KeyCode::Char('g')));
        assert_eq!(a.selected_index(), Some(0));
    }

    #[test]
    fn x_asks_before_killing_and_esc_cancels() {
        let mut a = app();
        a.handle_key(key(KeyCode::Char('x')));
        assert_eq!(
            a.mode,
            Mode::Kill {
                pid: 30,
                name: "cargo".into()
            }
        );
        a.handle_key(key(KeyCode::Esc));
        assert_eq!(a.mode, Mode::Normal);
    }

    #[test]
    fn refuses_to_kill_pid_1_and_itself() {
        let mut a = App::new("h".into());
        a.update(Metrics {
            procs: vec![row(1, "systemd", 9.0, 1)],
            ..Metrics::default()
        });
        a.handle_key(key(KeyCode::Char('x')));
        assert_eq!(a.mode, Mode::Normal);
        assert!(a.status.as_deref().unwrap().contains("PID 1"));
        let me = std::process::id();
        a.update(Metrics {
            procs: vec![row(me, "systui", 9.0, 1)],
            ..Metrics::default()
        });
        a.handle_key(key(KeyCode::Char('x')));
        assert_eq!(a.mode, Mode::Normal);
    }

    #[test]
    fn term_actually_stops_a_child() {
        let mut child = Command::new("sleep")
            .arg("60")
            .spawn()
            .expect("sleep exists");
        let mut a = App::new("h".into());
        a.mode = Mode::Kill {
            pid: child.id(),
            name: "sleep".into(),
        };
        a.handle_key(key(KeyCode::Enter));
        let status = child.wait().expect("child ends");
        assert!(!status.success());
        assert!(a.status.unwrap().contains("TERM"));
    }

    #[test]
    fn killing_a_missing_pid_reports_the_error() {
        assert!(send_signal(4_000_000, Signal::Term).is_err());
    }

    #[test]
    fn pause_freezes_samples_and_history() {
        let mut a = app();
        a.handle_key(key(KeyCode::Char(' ')));
        let before = a.cpu_hist.len();
        a.update(Metrics::default());
        assert_eq!(a.cpu_hist.len(), before);
        assert_eq!(a.metrics.procs.len(), 4);
    }

    #[test]
    fn interval_is_bounded() {
        let mut a = app();
        for _ in 0..10 {
            a.handle_key(key(KeyCode::Char('+')));
        }
        assert_eq!(a.interval_ms, MIN_INTERVAL_MS);
        for _ in 0..10 {
            a.handle_key(key(KeyCode::Char('-')));
        }
        assert_eq!(a.interval_ms, MAX_INTERVAL_MS);
    }

    #[test]
    fn history_is_a_ring() {
        let mut h = History::default();
        for i in 0..(HISTORY + 10) {
            h.push(i as f64);
        }
        assert_eq!(h.len(), HISTORY);
        assert_eq!(h.last(2), vec![(HISTORY + 8) as f64, (HISTORY + 9) as f64]);
        assert_eq!(h.last(HISTORY + 50).len(), HISTORY);
    }

    #[test]
    fn q_and_ctrl_c_quit() {
        let mut a = app();
        a.handle_key(key(KeyCode::Char('q')));
        assert!(a.quit);
        let mut b = app();
        b.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(b.quit);
        let mut c = app();
        c.handle_key(key(KeyCode::Char('c'))); // plain c is the sort key
        assert!(!c.quit);
    }
}
