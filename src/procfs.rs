//! Parsers for the files under `/proc`. Every parser is a pure function over
//! text, so it can be tested with fixtures; only [`read_snapshot`] touches the
//! filesystem.

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::time::Instant;

/// Cumulative jiffies of one CPU line in `/proc/stat`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CpuTimes {
    pub total: u64,
    /// `idle + iowait`: time the CPU had nothing to run.
    pub idle: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MemInfo {
    pub total_kb: u64,
    pub available_kb: u64,
    pub swap_total_kb: u64,
    pub swap_free_kb: u64,
}

impl MemInfo {
    pub fn used_kb(&self) -> u64 {
        self.total_kb.saturating_sub(self.available_kb)
    }
    pub fn swap_used_kb(&self) -> u64 {
        self.swap_total_kb.saturating_sub(self.swap_free_kb)
    }
}

/// What one process reports at a single instant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawProc {
    pub pid: u32,
    pub name: String,
    pub cmd: String,
    pub state: char,
    pub uid: u32,
    pub rss_kb: u64,
    /// `utime + stime` in jiffies since the process started.
    pub ticks: u64,
}

/// Everything read from `/proc` in one pass.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub at: Instant,
    pub cpu: CpuTimes,
    pub cores: Vec<CpuTimes>,
    pub mem: MemInfo,
    pub load: (f32, f32, f32),
    pub uptime_s: u64,
    pub net_rx: u64,
    pub net_tx: u64,
    pub disk_read: u64,
    pub disk_write: u64,
    pub procs: Vec<RawProc>,
}

fn cpu_line(line: &str) -> Option<CpuTimes> {
    let nums: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .map(|v| v.parse().unwrap_or(0))
        .collect();
    if nums.len() < 4 {
        return None;
    }
    // user nice system idle iowait irq softirq steal guest guest_nice. The
    // kernel already counts guest time inside user/nice, so it is not added.
    let total: u64 = nums.iter().take(8).sum();
    let idle = nums[3] + nums.get(4).copied().unwrap_or(0);
    Some(CpuTimes { total, idle })
}

/// Aggregate line and one entry per core, from `/proc/stat`.
pub fn parse_stat(text: &str) -> (CpuTimes, Vec<CpuTimes>) {
    let mut agg = CpuTimes::default();
    let mut cores = Vec::new();
    for line in text.lines() {
        let Some(label) = line.split_whitespace().next() else {
            continue;
        };
        if label == "cpu" {
            agg = cpu_line(line).unwrap_or_default();
        } else if label.starts_with("cpu")
            && label[3..].bytes().all(|b| b.is_ascii_digit())
            && let Some(c) = cpu_line(line)
        {
            cores.push(c);
        }
    }
    (agg, cores)
}

pub fn parse_meminfo(text: &str) -> MemInfo {
    let mut m = MemInfo::default();
    for line in text.lines() {
        let mut it = line.split_whitespace();
        let (Some(key), Some(val)) = (it.next(), it.next()) else {
            continue;
        };
        let val: u64 = val.parse().unwrap_or(0);
        match key {
            "MemTotal:" => m.total_kb = val,
            "MemAvailable:" => m.available_kb = val,
            "SwapTotal:" => m.swap_total_kb = val,
            "SwapFree:" => m.swap_free_kb = val,
            _ => {}
        }
    }
    m
}

pub fn parse_loadavg(text: &str) -> (f32, f32, f32) {
    let mut it = text.split_whitespace().map(|v| v.parse().unwrap_or(0.0));
    (
        it.next().unwrap_or(0.0),
        it.next().unwrap_or(0.0),
        it.next().unwrap_or(0.0),
    )
}

pub fn parse_uptime(text: &str) -> u64 {
    text.split_whitespace()
        .next()
        .and_then(|v| v.parse::<f64>().ok())
        .map(|v| v as u64)
        .unwrap_or(0)
}

/// Total received/transmitted bytes of every interface except loopback.
pub fn parse_net_dev(text: &str) -> (u64, u64) {
    let (mut rx, mut tx) = (0u64, 0u64);
    for line in text.lines().skip(2) {
        let Some((name, rest)) = line.split_once(':') else {
            continue;
        };
        if name.trim() == "lo" {
            continue;
        }
        let f: Vec<u64> = rest
            .split_whitespace()
            .map(|v| v.parse().unwrap_or(0))
            .collect();
        if f.len() >= 9 {
            rx += f[0];
            tx += f[8];
        }
    }
    (rx, tx)
}

/// Bytes read/written, summed over the devices accepted by `is_disk`
/// (partitions would be counted twice, so the caller filters them out).
/// Sectors in `/proc/diskstats` are always 512 bytes.
pub fn parse_diskstats(text: &str, is_disk: impl Fn(&str) -> bool) -> (u64, u64) {
    let (mut r, mut w) = (0u64, 0u64);
    for line in text.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 10 || !is_disk(f[2]) {
            continue;
        }
        r += f[5].parse::<u64>().unwrap_or(0) * 512;
        w += f[9].parse::<u64>().unwrap_or(0) * 512;
    }
    (r, w)
}

/// `(name, state, utime + stime)` from `/proc/<pid>/stat`. The name sits
/// between the first `(` and the *last* `)` because it may contain spaces and
/// parentheses itself.
pub fn parse_pid_stat(text: &str) -> Option<(String, char, u64)> {
    let open = text.find('(')?;
    let close = text.rfind(')')?;
    if close < open {
        return None;
    }
    let name = text[open + 1..close].to_string();
    let rest: Vec<&str> = text[close + 1..].split_whitespace().collect();
    // After the name: state is field 0, utime is field 11, stime field 12.
    let state = rest.first()?.chars().next()?;
    let utime: u64 = rest.get(11)?.parse().ok()?;
    let stime: u64 = rest.get(12)?.parse().ok()?;
    Some((name, state, utime + stime))
}

/// `(real uid, resident kB)` from `/proc/<pid>/status`. Kernel threads have no
/// `VmRSS` line, so their memory is 0.
pub fn parse_pid_status(text: &str) -> (u32, u64) {
    let (mut uid, mut rss) = (0, 0);
    for line in text.lines() {
        if let Some(v) = line.strip_prefix("Uid:") {
            uid = v
                .split_whitespace()
                .next()
                .and_then(|x| x.parse().ok())
                .unwrap_or(0);
        } else if let Some(v) = line.strip_prefix("VmRSS:") {
            rss = v
                .split_whitespace()
                .next()
                .and_then(|x| x.parse().ok())
                .unwrap_or(0);
        }
    }
    (uid, rss)
}

/// `/proc/<pid>/cmdline` separates arguments with NUL bytes.
pub fn parse_cmdline(raw: &[u8]) -> String {
    let s = String::from_utf8_lossy(raw);
    s.split('\0')
        .filter(|a| !a.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// uid → user name from the text of `/etc/passwd`.
pub fn parse_passwd(text: &str) -> HashMap<u32, String> {
    text.lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split(':').collect();
            Some((f.get(2)?.parse().ok()?, f.first()?.to_string()))
        })
        .collect()
}

fn read(root: &Path, rel: &str) -> String {
    fs::read_to_string(root.join(rel)).unwrap_or_default()
}

/// A whole disk (not a partition) is any block device that has an entry
/// directly under `/sys/block`; loop and ram devices are ignored.
fn is_whole_disk(name: &str) -> bool {
    !name.starts_with("loop")
        && !name.starts_with("ram")
        && Path::new("/sys/block").join(name).exists()
}

fn read_proc(root: &Path, pid: u32) -> Option<RawProc> {
    let dir = format!("{pid}");
    let (name, state, ticks) =
        parse_pid_stat(&fs::read_to_string(root.join(&dir).join("stat")).ok()?)?;
    let (uid, rss_kb) = parse_pid_status(&read(root, &format!("{dir}/status")));
    let cmd = parse_cmdline(&fs::read(root.join(&dir).join("cmdline")).unwrap_or_default());
    // Kernel threads have an empty command line; show them like top does.
    let cmd = if cmd.is_empty() {
        format!("[{name}]")
    } else {
        cmd
    };
    Some(RawProc {
        pid,
        name,
        cmd,
        state,
        uid,
        rss_kb,
        ticks,
    })
}

/// Read everything from `root` (normally `/proc`). A process that exits
/// between listing and reading is simply skipped.
pub fn read_snapshot(root: &Path) -> Snapshot {
    let (cpu, cores) = parse_stat(&read(root, "stat"));
    let (net_rx, net_tx) = parse_net_dev(&read(root, "net/dev"));
    let (disk_read, disk_write) = parse_diskstats(&read(root, "diskstats"), is_whole_disk);
    let mut procs: Vec<RawProc> = fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
        .filter_map(|pid| read_proc(root, pid))
        .collect();
    procs.sort_by_key(|p| p.pid);
    Snapshot {
        at: Instant::now(),
        cpu,
        cores,
        mem: parse_meminfo(&read(root, "meminfo")),
        load: parse_loadavg(&read(root, "loadavg")),
        uptime_s: parse_uptime(&read(root, "uptime")),
        net_rx,
        net_tx,
        disk_read,
        disk_write,
        procs,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STAT: &str = "cpu  100 10 50 800 40 5 5 0 0 0\ncpu0 60 5 20 400 20 2 3 0 0 0\ncpu1 40 5 30 400 20 3 2 0 0 0\nintr 1 2 3\nctxt 99\n";

    #[test]
    fn stat_aggregate_and_cores() {
        let (agg, cores) = parse_stat(STAT);
        assert_eq!(
            agg,
            CpuTimes {
                total: 1010,
                idle: 840
            }
        );
        assert_eq!(cores.len(), 2);
        assert_eq!(
            cores[1],
            CpuTimes {
                total: 500,
                idle: 420
            }
        );
    }

    #[test]
    fn stat_ignores_non_cpu_lines_that_start_with_cpu() {
        let (_, cores) = parse_stat("cpu 1 1 1 1\ncpufreq 9 9 9 9\ncpu0 1 1 1 1\n");
        assert_eq!(cores.len(), 1);
    }

    #[test]
    fn meminfo_reads_the_four_fields() {
        let m = parse_meminfo(
            "MemTotal:       16000 kB\nMemFree: 1 kB\nMemAvailable:    6000 kB\nSwapTotal: 4000 kB\nSwapFree: 3000 kB\n",
        );
        assert_eq!(m.used_kb(), 10000);
        assert_eq!(m.swap_used_kb(), 1000);
    }

    #[test]
    fn loadavg_and_uptime() {
        assert_eq!(
            parse_loadavg("0.52 0.40 0.31 1/500 1234\n"),
            (0.52, 0.40, 0.31)
        );
        assert_eq!(parse_uptime("12345.67 99999.00\n"), 12345);
        assert_eq!(parse_loadavg(""), (0.0, 0.0, 0.0));
    }

    #[test]
    fn net_dev_skips_loopback_and_headers() {
        let t = "Inter-|   Receive\n face |bytes packets errs drop fifo frame compressed multicast|bytes packets errs drop fifo colls carrier compressed\n    lo: 1000 1 0 0 0 0 0 0 1000 1 0 0 0 0 0 0\n  eth0: 5000 5 0 0 0 0 0 0 2000 2 0 0 0 0 0 0\n wlan0:  300 5 0 0 0 0 0 0  100 2 0 0 0 0 0 0\n";
        assert_eq!(parse_net_dev(t), (5300, 2100));
    }

    #[test]
    fn diskstats_counts_sectors_of_accepted_devices_only() {
        let t = "   8  0 sda 10 0 100 5 20 0 200 7 0 0 0\n   8  1 sda1 9 0 90 5 19 0 190 7 0 0 0\n";
        assert_eq!(parse_diskstats(t, |n| n == "sda"), (100 * 512, 200 * 512));
    }

    #[test]
    fn pid_stat_handles_spaces_and_parens_in_the_name() {
        let t = "42 (Web Content (x)) S 1 42 42 0 -1 4194560 0 0 0 0 7 3 0 0 20 0 1 0 100 0 0";
        assert_eq!(parse_pid_stat(t), Some(("Web Content (x)".into(), 'S', 10)));
        assert_eq!(parse_pid_stat("garbage"), None);
        assert_eq!(parse_pid_stat("1 (a) S 1 2"), None);
    }

    #[test]
    fn pid_status_kernel_thread_has_no_rss() {
        assert_eq!(
            parse_pid_status("Name:\tkthreadd\nUid:\t0\t0\t0\t0\n"),
            (0, 0)
        );
        assert_eq!(
            parse_pid_status("Uid:\t1000\t1000\t1000\t1000\nVmRSS:\t  2048 kB\n"),
            (1000, 2048)
        );
    }

    #[test]
    fn cmdline_joins_nul_separated_args() {
        assert_eq!(parse_cmdline(b"vim\0-O\0a.txt\0"), "vim -O a.txt");
        assert_eq!(parse_cmdline(b""), "");
    }

    #[test]
    fn passwd_maps_uid_to_name() {
        let m = parse_passwd(
            "root:x:0:0:root:/root:/bin/bash\nadrian:x:1000:1000::/home/adrian:/bin/fish\n#bad\n",
        );
        assert_eq!(m.get(&1000).map(String::as_str), Some("adrian"));
        assert_eq!(m.get(&0).map(String::as_str), Some("root"));
    }

    #[test]
    fn real_proc_has_this_process() {
        let snap = read_snapshot(Path::new("/proc"));
        assert!(snap.cpu.total > 0);
        assert!(!snap.cores.is_empty());
        assert!(snap.mem.total_kb > 0);
        let me = std::process::id();
        let p = snap
            .procs
            .iter()
            .find(|p| p.pid == me)
            .expect("own pid listed");
        assert!(p.rss_kb > 0);
        assert!(!p.cmd.is_empty());
    }
}
