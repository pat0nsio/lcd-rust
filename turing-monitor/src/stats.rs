// SPDX-License-Identifier: GPL-3.0-or-later
//! System metrics read straight from /proc and /sys.
//!
//! No sampling threads and no allocation in the steady state: every sampler
//! owns a reusable String buffer and parses in place.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Instant;

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
