// SPDX-License-Identifier: GPL-3.0-or-later
//! System metrics read straight from /proc and /sys.
//!
//! No sampling threads and no allocation in the steady state: every sampler
//! owns a reusable String buffer and parses in place.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Read a file into a reusable buffer, avoiding a fresh allocation per tick.
fn read_into(path: &Path, buf: &mut String) -> std::io::Result<()> {
    buf.clear();
    File::open(path)?.read_to_string(buf)?;
    Ok(())
}

fn read_trimmed(path: impl AsRef<Path>) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
}

fn read_u64(path: impl AsRef<Path>) -> Option<u64> {
    read_trimmed(path)?.parse().ok()
}

// ---------------------------------------------------------------- CPU

#[derive(Clone, Copy, Default)]
struct CpuTimes {
    idle: u64,
    total: u64,
}

impl CpuTimes {
    /// Parse one `cpu...` line of /proc/stat.
    fn parse(line: &str) -> Option<CpuTimes> {
        let mut idle = 0;
        let mut total = 0;
        for (i, f) in line.split_ascii_whitespace().skip(1).enumerate() {
            let v: u64 = f.parse().ok()?;
            total += v;
            // Fields 3 and 4 are idle and iowait.
            if i == 3 || i == 4 {
                idle += v;
            }
        }
        Some(CpuTimes { idle, total })
    }

    /// Busy fraction between two samples.
    fn usage_since(&self, prev: &CpuTimes) -> f32 {
        let dt = self.total.saturating_sub(prev.total);
        if dt == 0 {
            return 0.0;
        }
        let di = self.idle.saturating_sub(prev.idle);
        (1.0 - di as f32 / dt as f32).clamp(0.0, 1.0)
    }
}

pub struct CpuSampler {
    prev_total: CpuTimes,
    prev_cores: Vec<CpuTimes>,
    buf: String,
    /// Busy fraction per logical core, 0..=1.
    pub cores: Vec<f32>,
    /// Overall busy fraction, 0..=1.
    pub usage: f32,
}

impl CpuSampler {
    pub fn new() -> Self {
        let mut s = CpuSampler {
            prev_total: CpuTimes::default(),
            prev_cores: Vec::new(),
            buf: String::with_capacity(4096),
            cores: Vec::new(),
            usage: 0.0,
        };
        s.sample(); // Prime the deltas; the first reading is always zero.
        s
    }

    pub fn sample(&mut self) {
        if read_into(Path::new("/proc/stat"), &mut self.buf).is_err() {
            return;
        }
        let mut core_idx = 0;
        // Take the borrow of buf separately so the fields can be written.
        let (mut total_now, mut cores_now) = (None, Vec::new());
        for line in self.buf.lines() {
            if !line.starts_with("cpu") {
                break; // The cpu lines always come first.
            }
            let Some(t) = CpuTimes::parse(line) else {
                continue;
            };
            if line.as_bytes().get(3) == Some(&b' ') {
                total_now = Some(t);
            } else {
                cores_now.push(t);
                core_idx += 1;
            }
        }

        if let Some(t) = total_now {
            self.usage = t.usage_since(&self.prev_total);
            self.prev_total = t;
        }
        if self.prev_cores.len() != core_idx {
            self.prev_cores = vec![CpuTimes::default(); core_idx];
            self.cores = vec![0.0; core_idx];
        }
        for (i, t) in cores_now.into_iter().enumerate() {
            self.cores[i] = t.usage_since(&self.prev_cores[i]);
            self.prev_cores[i] = t;
        }
    }

    /// Mean of the per-core scaling frequencies, in MHz.
    pub fn frequency_mhz(&self) -> Option<u32> {
        let mut sum = 0u64;
        let mut n = 0u64;
        for i in 0..self.cores.len() {
            let p = format!("/sys/devices/system/cpu/cpu{i}/cpufreq/scaling_cur_freq");
            if let Some(khz) = read_u64(&p) {
                sum += khz;
                n += 1;
            }
        }
        (n > 0).then(|| (sum / n / 1000) as u32)
    }
}

// ---------------------------------------------------------------- memory

#[derive(Clone, Copy, Default)]
pub struct Memory {
    pub total_kb: u64,
    pub available_kb: u64,
    pub swap_total_kb: u64,
    pub swap_free_kb: u64,
}

impl Memory {
    pub fn used_kb(&self) -> u64 {
        self.total_kb.saturating_sub(self.available_kb)
    }

    pub fn usage(&self) -> f32 {
        if self.total_kb == 0 {
            return 0.0;
        }
        self.used_kb() as f32 / self.total_kb as f32
    }

    pub fn swap_used_kb(&self) -> u64 {
        self.swap_total_kb.saturating_sub(self.swap_free_kb)
    }

    pub fn swap_usage(&self) -> f32 {
        if self.swap_total_kb == 0 {
            return 0.0;
        }
        self.swap_used_kb() as f32 / self.swap_total_kb as f32
    }
}

pub struct MemSampler {
    buf: String,
    pub mem: Memory,
}

impl MemSampler {
    pub fn new() -> Self {
        MemSampler {
            buf: String::with_capacity(2048),
            mem: Memory::default(),
        }
    }

    pub fn sample(&mut self) {
        if read_into(Path::new("/proc/meminfo"), &mut self.buf).is_err() {
            return;
        }
        let mut m = Memory::default();
        for line in self.buf.lines() {
            let Some((key, rest)) = line.split_once(':') else {
                continue;
            };
            let Some(v) = rest
                .split_ascii_whitespace()
                .next()
                .and_then(|v| v.parse::<u64>().ok())
            else {
                continue;
            };
            match key {
                "MemTotal" => m.total_kb = v,
                "MemAvailable" => m.available_kb = v,
                "SwapTotal" => m.swap_total_kb = v,
                "SwapFree" => m.swap_free_kb = v,
                _ => {}
            }
        }
        self.mem = m;
    }
}

// ---------------------------------------------------------------- sensors

/// A resolved hwmon temperature input, found once at startup.
pub struct TempSensor {
    path: PathBuf,
}

impl TempSensor {
    /// Locate `tempN_input` under the hwmon device named `chip`, optionally
    /// requiring a matching `tempN_label` (e.g. "edge", "Tctl").
    pub fn find(chip: &str, label: Option<&str>) -> Option<TempSensor> {
        for entry in std::fs::read_dir("/sys/class/hwmon").ok()? {
            let dir = entry.ok()?.path();
            if read_trimmed(dir.join("name")).as_deref() != Some(chip) {
                continue;
            }
            for n in 1..=8 {
                let input = dir.join(format!("temp{n}_input"));
                if !input.is_file() {
                    continue;
                }
                match label {
                    None => return Some(TempSensor { path: input }),
                    Some(want) => {
                        let got = read_trimmed(dir.join(format!("temp{n}_label")));
                        if got.as_deref() == Some(want) {
                            return Some(TempSensor { path: input });
                        }
                    }
                }
            }
        }
        None
    }

    /// Current reading in degrees Celsius.
    pub fn read(&self) -> Option<f32> {
        read_u64(&self.path).map(|milli| milli as f32 / 1000.0)
    }
}

// ---------------------------------------------------------------- GPU

/// AMD GPU counters exposed by the amdgpu driver through sysfs.
pub struct AmdGpu {
    device: PathBuf,
    pub temp: Option<TempSensor>,
}

impl AmdGpu {
    pub fn find() -> Option<AmdGpu> {
        for entry in std::fs::read_dir("/sys/class/drm").ok()? {
            let card = entry.ok()?.path();
            let name = card.file_name()?.to_string_lossy().into_owned();
            // Skip connector nodes like card1-HDMI-A-1.
            if !name.starts_with("card") || name.contains('-') {
                continue;
            }
            let device = card.join("device");
            if device.join("gpu_busy_percent").is_file() {
                return Some(AmdGpu {
                    device,
                    temp: TempSensor::find("amdgpu", Some("edge")),
                });
            }
        }
        None
    }

    /// Busy fraction, 0..=1.
    pub fn usage(&self) -> f32 {
        read_u64(self.device.join("gpu_busy_percent"))
            .map(|p| (p as f32 / 100.0).clamp(0.0, 1.0))
            .unwrap_or(0.0)
    }

    /// (used, total) VRAM in bytes.
    pub fn vram(&self) -> Option<(u64, u64)> {
        let used = read_u64(self.device.join("mem_info_vram_used"))?;
        let total = read_u64(self.device.join("mem_info_vram_total"))?;
        Some((used, total))
    }
}

// ---------------------------------------------------------------- FPS

/// A log MangoHud is writing has been touched within this long. Past it the
/// game is assumed gone and the metric disappears, so a theme's `require`
/// drops the FPS section instead of freezing on the last frame rate seen.
const FPS_STALE: Duration = Duration::from_secs(10);

/// Only ever read this much from the end of a log. MangoHud appends for as
/// long as the game runs, so the file is unbounded but the last row is not.
const FPS_TAIL: u64 = 8 * 1024;

/// Live frame rate, read from the CSV MangoHud appends to while a game runs.
///
/// The log has three header lines — system keys, system values, then the
/// column names — followed by one row per `log_interval`. MangoHud flushes as
/// it goes, so tailing the newest file gives the current frame rate.
pub struct MangoHudFps {
    folder: PathBuf,
    /// The log being followed, and which column of it holds the frame rate.
    current: Option<(PathBuf, usize)>,
    /// Reusable tail buffer, so a steady state costs no allocation.
    buf: String,
}

impl MangoHudFps {
    pub fn new(folder: &str) -> MangoHudFps {
        MangoHudFps {
            folder: PathBuf::from(folder),
            current: None,
            buf: String::with_capacity(FPS_TAIL as usize),
        }
    }

    /// Frames per second, or `None` when nothing is logging right now.
    pub fn read(&mut self) -> Option<f32> {
        // While the log we are already following keeps being written, one
        // stat is enough. `autostart_log` leaves a file per game session and
        // never cleans up, so scanning the folder every tick would get slower
        // the longer the folder has existed.
        let path = match self.current.take() {
            Some((p, col)) if is_fresh(&p) => {
                self.current = Some((p.clone(), col));
                p
            }
            _ => {
                let p = self.newest_log()?;
                let col = fps_column(&p)?;
                self.current = Some((p.clone(), col));
                p
            }
        };
        let col = self.current.as_ref()?.1;
        read_tail(&path, FPS_TAIL, &mut self.buf)?;
        last_field(&self.buf, col)
    }

    /// The name of the program being logged, taken from the log's filename.
    pub fn app(&self) -> Option<String> {
        let stem = self.current.as_ref()?.0.file_stem()?.to_str()?;
        // MangoHud names logs `<program>_<date>_<time>.csv`.
        Some(stem.rsplitn(3, '_').last()?.to_string())
    }

    /// Most recently written `.csv` in the folder, if it is still being written.
    fn newest_log(&self) -> Option<PathBuf> {
        let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
        for entry in std::fs::read_dir(&self.folder).ok()? {
            let path = entry.ok()?.path();
            if path.extension().and_then(|e| e.to_str()) != Some("csv") {
                continue;
            }
            let Some(modified) = fresh_mtime(&path) else {
                continue;
            };
            if best.as_ref().is_none_or(|(t, _)| modified > *t) {
                best = Some((modified, path));
            }
        }
        best.map(|(_, p)| p)
    }
}

/// Modification time, but only if it is recent enough to mean "still being
/// written". `None` covers both a missing file and an abandoned one.
fn fresh_mtime(path: &Path) -> Option<std::time::SystemTime> {
    let modified = path.metadata().and_then(|md| md.modified()).ok()?;
    let age = std::time::SystemTime::now()
        .duration_since(modified)
        .unwrap_or_default();
    (age <= FPS_STALE).then_some(modified)
}

fn is_fresh(path: &Path) -> bool {
    fresh_mtime(path).is_some()
}

/// Index of the `fps` column, from the log's third line.
fn fps_column(path: &Path) -> Option<usize> {
    let head = read_head(path, 4096)?;
    head.lines()
        .find_map(|line| line.split(',').position(|f| f.trim() == "fps"))
}

/// Field `col` of the last complete line, parsed as a number.
fn last_field(text: &str, col: usize) -> Option<f32> {
    // The final line may be half-written, so only trust lines ending in \n.
    let complete = &text[..text.rfind('\n')?];
    complete
        .lines()
        .rev()
        .find_map(|line| line.split(',').nth(col)?.trim().parse::<f32>().ok())
}

fn read_head(path: &Path, max: usize) -> Option<String> {
    let mut buf = vec![0u8; max];
    let n = File::open(path).ok()?.read(&mut buf).ok()?;
    buf.truncate(n);
    String::from_utf8(buf).ok()
}

/// Read the last `max` bytes of a file into `out`. A cut in the middle of a
/// line is harmless: the caller only looks at the last complete one.
fn read_tail(path: &Path, max: u64, out: &mut String) -> Option<()> {
    let mut f = File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    f.seek(SeekFrom::Start(len.saturating_sub(max))).ok()?;
    out.clear();
    // A log is ASCII, so reading straight into a String is safe; a truncated
    // multi-byte character would only come from a corrupt file.
    f.read_to_string(out).ok()?;
    Some(())
}

// ---------------------------------------------------------------- network

pub struct NetSampler {
    iface: String,
    buf: String,
    prev: Option<(u64, u64, Instant)>,
    /// Bytes per second since the previous sample.
    pub rx_rate: f64,
    pub tx_rate: f64,
}

impl NetSampler {
    /// Uses `iface` if given, otherwise the busiest non-loopback interface.
    pub fn new(iface: Option<&str>) -> Self {
        let iface = iface
            .map(str::to_string)
            .or_else(default_interface)
            .unwrap_or_default();
        let mut s = NetSampler {
            iface,
            buf: String::with_capacity(4096),
            prev: None,
            rx_rate: 0.0,
            tx_rate: 0.0,
        };
        s.sample();
        s
    }

    pub fn interface(&self) -> &str {
        &self.iface
    }

    pub fn sample(&mut self) {
        if self.iface.is_empty() || read_into(Path::new("/proc/net/dev"), &mut self.buf).is_err() {
            return;
        }
        let mut found = None;
        for line in self.buf.lines() {
            let Some((name, rest)) = line.split_once(':') else {
                continue;
            };
            if name.trim() != self.iface {
                continue;
            }
            let f: Vec<&str> = rest.split_ascii_whitespace().collect();
            // Receive bytes is field 0, transmit bytes is field 8.
            if let (Some(rx), Some(tx)) = (f.first(), f.get(8)) {
                if let (Ok(rx), Ok(tx)) = (rx.parse::<u64>(), tx.parse::<u64>()) {
                    found = Some((rx, tx));
                }
            }
            break;
        }
        let Some((rx, tx)) = found else { return };

        let now = Instant::now();
        if let Some((prx, ptx, pt)) = self.prev {
            let dt = now.duration_since(pt).as_secs_f64();
            if dt > 0.0 {
                // Counters wrap and reset; saturating_sub reports 0 rather
                // than a nonsense spike.
                self.rx_rate = rx.saturating_sub(prx) as f64 / dt;
                self.tx_rate = tx.saturating_sub(ptx) as f64 / dt;
            }
        }
        self.prev = Some((rx, tx, now));
    }
}

fn default_interface() -> Option<String> {
    let mut best: Option<(u64, String)> = None;
    for entry in std::fs::read_dir("/sys/class/net").ok()? {
        let dir = entry.ok()?.path();
        let name = dir.file_name()?.to_string_lossy().into_owned();
        if name == "lo" {
            continue;
        }
        if read_trimmed(dir.join("operstate")).as_deref() != Some("up") {
            continue;
        }
        let rx = read_u64(dir.join("statistics/rx_bytes")).unwrap_or(0);
        if best.as_ref().is_none_or(|(b, _)| rx > *b) {
            best = Some((rx, name));
        }
    }
    best.map(|(_, n)| n)
}

// ---------------------------------------------------------------- disk

#[derive(Clone, Copy, Default)]
pub struct Disk {
    pub total: u64,
    pub free: u64,
}

impl Disk {
    pub fn used(&self) -> u64 {
        self.total.saturating_sub(self.free)
    }

    pub fn usage(&self) -> f32 {
        if self.total == 0 {
            return 0.0;
        }
        self.used() as f32 / self.total as f32
    }
}

/// Capacity of the filesystem holding `mount_point`.
pub fn disk_usage(mount_point: &str) -> Disk {
    let Ok(path) = std::ffi::CString::new(mount_point) else {
        return Disk::default();
    };
    // SAFETY: statvfs only writes into the zeroed struct we hand it, and the
    // path is a valid NUL-terminated string for the duration of the call.
    unsafe {
        let mut st: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(path.as_ptr(), &mut st) != 0 {
            return Disk::default();
        }
        // f_bavail, not f_bfree: reserved blocks aren't usable space.
        Disk {
            total: st.f_blocks as u64 * st.f_frsize as u64,
            free: st.f_bavail as u64 * st.f_frsize as u64,
        }
    }
}

// ---------------------------------------------------------------- misc

pub fn uptime_secs() -> u64 {
    read_trimmed("/proc/uptime")
        .and_then(|s| s.split('.').next().and_then(|v| v.parse().ok()))
        .unwrap_or(0)
}

pub fn load_average() -> (f32, f32, f32) {
    let Some(s) = read_trimmed("/proc/loadavg") else {
        return (0.0, 0.0, 0.0);
    };
    let mut it = s.split_ascii_whitespace();
    let mut next = || it.next().and_then(|v| v.parse().ok()).unwrap_or(0.0);
    (next(), next(), next())
}

pub fn hostname() -> String {
    read_trimmed("/proc/sys/kernel/hostname").unwrap_or_else(|| "linux".into())
}

pub fn cpu_model() -> String {
    let Ok(s) = std::fs::read_to_string("/proc/cpuinfo") else {
        return "CPU".into();
    };
    for line in s.lines() {
        if let Some((k, v)) = line.split_once(':') {
            if k.trim() == "model name" {
                // Trim the marketing noise so it fits a 320px-wide panel.
                return v
                    .trim()
                    .replace("(R)", "")
                    .replace("(TM)", "")
                    .replace(" with Radeon Graphics", "")
                    .replace(" CPU", "")
                    .trim()
                    .to_string();
            }
        }
    }
    "CPU".into()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape MangoHud actually writes: system keys, system values, the
    /// column names, then one row per interval.
    const LOG: &str = "\
os,cpu,gpu,ram,kernel,driver,cpuscheduler
CachyOS,AMD Ryzen 5 5600G,AMD Radeon RX 6650 XT,32745780,7.1.8,,performance
fps,frametime,cpu_load,gpu_load,cpu_temp,gpu_temp,elapsed
59.2106,16.8889,1.67224,4,37,30,500463843
60.1332,16.6298,2.66223,4,37,30,1000509893
143.907,6.94,12.5,88,61,58,1500524935
";

    #[test]
    fn the_frame_rate_is_the_last_complete_row() {
        let col = LOG
            .lines()
            .find_map(|l| l.split(',').position(|f| f == "fps"))
            .unwrap();
        assert_eq!(col, 0);
        assert_eq!(last_field(LOG, col), Some(143.907));
    }

    #[test]
    fn a_half_written_final_row_is_ignored() {
        // MangoHud appends while we read, so the tail can end mid-number.
        let partial = format!("{LOG}61.5,16.2,3.1");
        assert_eq!(last_field(&partial, 0), Some(143.907));
    }

    #[test]
    fn a_followed_log_is_not_rescanned_while_it_stays_fresh() {
        let dir = std::env::temp_dir().join("turing-lcd-rs-fps-follow");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("game_2026-08-20_18-35-46.csv");
        std::fs::write(&path, LOG).unwrap();

        let mut src = MangoHudFps::new(dir.to_str().unwrap());
        assert_eq!(src.read(), Some(143.907));
        // A second log appearing must not steal the one already followed.
        std::fs::write(dir.join("other_2026-08-20_19-00-00.csv"), LOG).unwrap();
        assert_eq!(src.app().as_deref(), Some("game"));
        assert_eq!(src.read(), Some(143.907));
        assert_eq!(src.app().as_deref(), Some("game"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_tail_cut_mid_line_still_reads_the_last_row() {
        // read_tail seeks blindly, so the first line is often a fragment.
        let cut = &LOG[LOG.len() - 90..];
        assert_eq!(last_field(cut, 0), Some(143.907));
    }

    #[test]
    fn a_log_with_no_rows_yet_gives_nothing() {
        let headers = LOG.lines().take(3).collect::<Vec<_>>().join("\n") + "\n";
        assert_eq!(last_field(&headers, 0), None);
    }

    #[test]
    fn fps_column_is_found_wherever_it_sits() {
        let dir = std::env::temp_dir().join("turing-lcd-rs-fps-test");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("game_2026-08-20_18-35-46.csv");
        std::fs::write(&path, LOG).unwrap();
        assert_eq!(fps_column(&path), Some(0));

        let mut src = MangoHudFps::new(dir.to_str().unwrap());
        assert_eq!(src.read(), Some(143.907));
        // The program name comes off the filename, date and time stripped.
        assert_eq!(src.app().as_deref(), Some("game"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_folder_is_not_an_error() {
        let mut src = MangoHudFps::new("/nonexistent/mangologs");
        assert_eq!(src.read(), None);
    }
}
