//! Turns two snapshots into rates. `/proc` only holds cumulative counters, so
//! every percentage and every bytes/second figure is a difference between the
//! previous sample and the current one.

use std::collections::HashMap;

use crate::procfs::{CpuTimes, MemInfo, Snapshot};

#[derive(Debug, Clone, PartialEq)]
pub struct ProcRow {
    pub pid: u32,
    pub user: String,
    pub state: char,
    /// 100 % is one fully used core, as in `top`.
    pub cpu: f32,
    pub rss_kb: u64,
    pub name: String,
    pub cmd: String,
}

#[derive(Debug, Clone, Default)]
pub struct Metrics {
    pub cpu: f32,
    pub cores: Vec<f32>,
    pub mem: MemInfo,
    pub load: (f32, f32, f32),
    pub uptime_s: u64,
    pub net_rx_s: f64,
    pub net_tx_s: f64,
    pub disk_read_s: f64,
    pub disk_write_s: f64,
    pub procs: Vec<ProcRow>,
}

/// Busy share of the interval between two readings of the same CPU line.
pub fn cpu_percent(prev: CpuTimes, cur: CpuTimes) -> f32 {
    let total = cur.total.saturating_sub(prev.total);
    if total == 0 {
        return 0.0;
    }
    let idle = cur.idle.saturating_sub(prev.idle).min(total);
    (total - idle) as f32 * 100.0 / total as f32
}

fn rate(prev: u64, cur: u64, secs: f64) -> f64 {
    if secs <= 0.0 {
        return 0.0;
    }
    // A counter that goes backwards (interface reset) counts as no traffic.
    cur.saturating_sub(prev) as f64 / secs
}

pub fn compute(prev: &Snapshot, cur: &Snapshot, users: &HashMap<u32, String>) -> Metrics {
    let secs = cur.at.saturating_duration_since(prev.at).as_secs_f64();
    let cores: Vec<f32> = cur
        .cores
        .iter()
        .enumerate()
        .map(|(i, c)| prev.cores.get(i).map_or(0.0, |p| cpu_percent(*p, *c)))
        .collect();

    // Jiffies the whole machine could spend per core in this interval; a
    // process using one full core accrues exactly this many.
    let per_core_ticks =
        cur.cpu.total.saturating_sub(prev.cpu.total) as f32 / cur.cores.len().max(1) as f32;
    let before: HashMap<u32, u64> = prev.procs.iter().map(|p| (p.pid, p.ticks)).collect();

    let procs = cur
        .procs
        .iter()
        .map(|p| {
            let cpu = match before.get(&p.pid) {
                Some(&old) if per_core_ticks > 0.0 => {
                    p.ticks.saturating_sub(old) as f32 * 100.0 / per_core_ticks
                }
                _ => 0.0,
            };
            ProcRow {
                pid: p.pid,
                user: users
                    .get(&p.uid)
                    .cloned()
                    .unwrap_or_else(|| p.uid.to_string()),
                state: p.state,
                cpu,
                rss_kb: p.rss_kb,
                name: p.name.clone(),
                cmd: p.cmd.clone(),
            }
        })
        .collect();

    Metrics {
        cpu: cpu_percent(prev.cpu, cur.cpu),
        cores,
        mem: cur.mem,
        load: cur.load,
        uptime_s: cur.uptime_s,
        net_rx_s: rate(prev.net_rx, cur.net_rx, secs),
        net_tx_s: rate(prev.net_tx, cur.net_tx, secs),
        disk_read_s: rate(prev.disk_read, cur.disk_read, secs),
        disk_write_s: rate(prev.disk_write, cur.disk_write, secs),
        procs,
    }
}

/// 1024-based size with one decimal, e.g. `1.5G`. Input in kilobytes.
pub fn human_kb(kb: u64) -> String {
    human_bytes(kb as f64 * 1024.0)
}

pub fn human_bytes(b: f64) -> String {
    const UNITS: [&str; 5] = ["B", "K", "M", "G", "T"];
    let mut v = b.max(0.0);
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{:.0}B", v)
    } else {
        format!("{:.1}{}", v, UNITS[i])
    }
}

/// `3d 4h`, `2h 05m` or `7m 12s`: the two most significant units.
pub fn human_uptime(s: u64) -> String {
    let (d, h, m) = (s / 86_400, s % 86_400 / 3600, s % 3600 / 60);
    if d > 0 {
        format!("{d}d {h}h")
    } else if h > 0 {
        format!("{h}h {m:02}m")
    } else {
        format!("{m}m {:02}s", s % 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::procfs::RawProc;
    use std::time::{Duration, Instant};

    fn t(total: u64, idle: u64) -> CpuTimes {
        CpuTimes { total, idle }
    }

    fn raw(pid: u32, ticks: u64) -> RawProc {
        RawProc {
            pid,
            name: "x".into(),
            cmd: "x".into(),
            state: 'S',
            uid: 1000,
            rss_kb: 1,
            ticks,
        }
    }

    fn snap(at: Instant, cpu: CpuTimes, cores: Vec<CpuTimes>, procs: Vec<RawProc>) -> Snapshot {
        Snapshot {
            at,
            cpu,
            cores,
            mem: MemInfo::default(),
            load: (0.0, 0.0, 0.0),
            uptime_s: 0,
            net_rx: 0,
            net_tx: 0,
            disk_read: 0,
            disk_write: 0,
            procs,
        }
    }

    #[test]
    fn cpu_percent_is_busy_share_of_the_delta() {
        assert_eq!(cpu_percent(t(1000, 900), t(1100, 950)), 50.0);
        assert_eq!(cpu_percent(t(1000, 900), t(1100, 1000)), 0.0);
        assert_eq!(cpu_percent(t(1000, 900), t(1100, 900)), 100.0);
    }

    #[test]
    fn cpu_percent_survives_equal_or_backwards_counters() {
        assert_eq!(cpu_percent(t(5, 5), t(5, 5)), 0.0);
        assert_eq!(cpu_percent(t(100, 50), t(10, 5)), 0.0);
    }

    #[test]
    fn process_using_one_core_of_two_shows_100() {
        let t0 = Instant::now();
        let prev = snap(t0, t(0, 0), vec![t(0, 0), t(0, 0)], vec![raw(7, 100)]);
        // 1 s on 2 cores at 100 Hz = 200 jiffies; the process used 100 of them.
        let cur = snap(
            t0 + Duration::from_secs(1),
            t(200, 100),
            vec![t(100, 0), t(100, 100)],
            vec![raw(7, 200), raw(8, 5)],
        );
        let m = compute(&prev, &cur, &HashMap::from([(1000, "adrian".to_string())]));
        assert_eq!(m.cpu, 50.0);
        assert_eq!(m.cores, vec![100.0, 0.0]);
        assert_eq!(m.procs[0].cpu, 100.0);
        assert_eq!(m.procs[0].user, "adrian");
        // A process that was not in the previous sample has no delta yet.
        assert_eq!(m.procs[1].cpu, 0.0);
    }

    #[test]
    fn rates_divide_by_elapsed_seconds() {
        let t0 = Instant::now();
        let mut prev = snap(t0, t(0, 0), vec![], vec![]);
        prev.net_rx = 1000;
        prev.disk_write = 500;
        let mut cur = snap(t0 + Duration::from_millis(500), t(0, 0), vec![], vec![]);
        cur.net_rx = 2000;
        cur.disk_write = 400; // counter reset: must not underflow
        let m = compute(&prev, &cur, &HashMap::new());
        assert_eq!(m.net_rx_s, 2000.0);
        assert_eq!(m.disk_write_s, 0.0);
    }

    #[test]
    fn unknown_uid_falls_back_to_the_number() {
        let t0 = Instant::now();
        let s = snap(t0, t(0, 0), vec![], vec![raw(1, 0)]);
        assert_eq!(compute(&s, &s, &HashMap::new()).procs[0].user, "1000");
    }

    #[test]
    fn human_formats() {
        assert_eq!(human_bytes(0.0), "0B");
        assert_eq!(human_bytes(1536.0), "1.5K");
        assert_eq!(human_kb(1024 * 1024), "1.0G");
        assert_eq!(human_uptime(3 * 86_400 + 4 * 3600), "3d 4h");
        assert_eq!(human_uptime(2 * 3600 + 300), "2h 05m");
        assert_eq!(human_uptime(432), "7m 12s");
    }
}
