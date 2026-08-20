// SPDX-License-Identifier: GPL-3.0-or-later
//! Wire protocol for Turing Smart Screen rev. A and UsbMonitor panels.
//!
//! Ported from `library/lcd/lcd_comm_rev_a.py` of
//! <https://github.com/mathoudebine/turing-smart-screen-python>.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serialport::{FlowControl, SerialPort};

pub const VID: u16 = 0x1a86;
pub const PID: u16 = 0x5722;
pub const SERIAL: &str = "USB35INCHIPSV2";
const BAUD: u32 = 115_200;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
enum Command {
    Reset = 101,
    Clear = 102,
    ScreenOff = 108,
    ScreenOn = 109,
    SetBrightness = 110,
    SetOrientation = 121,
    DisplayBitmap = 197,
    Hello = 69,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Orientation {
    Portrait = 0,
    ReversePortrait = 1,
    Landscape = 2,
    ReverseLandscape = 3,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubRevision {
    /// Official Turing 3.5" — stays silent on HELLO.
    Turing35,
    UsbMonitor35,
    UsbMonitor5,
    UsbMonitor7,
}

impl SubRevision {
    /// Native panel size in portrait orientation.
    pub const fn size(self) -> (u16, u16) {
        match self {
            SubRevision::Turing35 | SubRevision::UsbMonitor35 => (320, 480),
            SubRevision::UsbMonitor5 => (480, 800),
            SubRevision::UsbMonitor7 => (600, 1024),
        }
    }
}

pub struct Device {
    port: Box<dyn SerialPort>,
    path: String,
    sub_revision: SubRevision,
    display_width: u16,
    display_height: u16,
    orientation: Orientation,
}

impl Device {
    /// Open the first matching panel, or the given port if `port` is `Some`.
    pub fn open(port: Option<&str>) -> Result<Self, String> {
        let path = match port {
            Some(p) => p.to_string(),
            None => detect_port().ok_or_else(|| {
                format!("no Turing panel found (looked for {VID:04x}:{PID:04x} / serial {SERIAL})")
            })?,
        };

        // Hardware flow control is what keeps the panel from being overrun:
        // it has no other way to tell us to slow down mid-bitmap.
        let port = serialport::new(&path, BAUD)
            .flow_control(FlowControl::Hardware)
            .timeout(Duration::from_secs(1))
            .open()
            .map_err(|e| format!("cannot open {path}: {e}"))?;

        let mut dev = Device {
            port,
            path,
            sub_revision: SubRevision::Turing35,
            display_width: 320,
            display_height: 480,
            orientation: Orientation::Portrait,
        };
        dev.hello()?;
        Ok(dev)
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    pub const fn sub_revision(&self) -> SubRevision {
        self.sub_revision
    }

    pub const fn orientation(&self) -> Orientation {
        self.orientation
    }

    /// Width in the current orientation.
    pub const fn width(&self) -> u16 {
        match self.orientation {
            Orientation::Portrait | Orientation::ReversePortrait => self.display_width,
            _ => self.display_height,
        }
    }

    /// Height in the current orientation.
    pub const fn height(&self) -> u16 {
        match self.orientation {
            Orientation::Portrait | Orientation::ReversePortrait => self.display_height,
            _ => self.display_width,
        }
    }

    /// Ask the panel which model it is. The official Turing 3.5" never
    /// answers, so a read timeout is a valid outcome, not an error.
    fn hello(&mut self) -> Result<(), String> {
        let probe = [Command::Hello as u8; 6];
        self.write(&probe)?;

        let mut resp = [0u8; 6];
        let got = read_exact_or_timeout(&mut *self.port, &mut resp);
        let _ = self.port.clear(serialport::ClearBuffer::Input);

        self.sub_revision = if got == 6 && resp == [0x01; 6] {
            SubRevision::UsbMonitor35
        } else if got == 6 && resp == [0x02; 6] {
            SubRevision::UsbMonitor5
        } else if got == 6 && resp == [0x03; 6] {
            SubRevision::UsbMonitor7
        } else {
            SubRevision::Turing35
        };
        let (w, h) = self.sub_revision.size();
        self.display_width = w;
        self.display_height = h;
        Ok(())
    }

    fn write(&mut self, data: &[u8]) -> Result<(), String> {
        self.port
            .write_all(data)
            .map_err(|e| format!("serial write failed: {e}"))
    }

    /// The 6-byte command frame: four 10-bit coordinates packed big-endian
    /// across five bytes, then the opcode.
    fn send_command(&mut self, cmd: Command, x: u16, y: u16, ex: u16, ey: u16) -> Result<(), String> {
        let frame = [
            (x >> 2) as u8,
            (((x & 3) << 6) | (y >> 4)) as u8,
            (((y & 15) << 4) | (ex >> 6)) as u8,
            (((ex & 63) << 2) | (ey >> 8)) as u8,
            (ey & 255) as u8,
            cmd as u8,
        ];
        self.write(&frame)
    }

    /// Reboot the panel. The port disappears and comes back, so the caller
    /// must reopen afterwards.
    pub fn reset(&mut self) -> Result<(), String> {
        self.send_command(Command::Reset, 0, 0, 0, 0)
    }

    pub fn clear(&mut self) -> Result<(), String> {
        // The panel only clears correctly from portrait; restore afterwards.
        let saved = self.orientation;
        self.set_orientation(Orientation::Portrait)?;
        self.send_command(Command::Clear, 0, 0, 0, 0)?;
        self.set_orientation(saved)
    }

    pub fn screen_on(&mut self) -> Result<(), String> {
        self.send_command(Command::ScreenOn, 0, 0, 0, 0)
    }

    pub fn screen_off(&mut self) -> Result<(), String> {
        self.send_command(Command::ScreenOff, 0, 0, 0, 0)
    }

    /// `level` is 0-100 percent. The panel itself runs inverted, 0 brightest.
    pub fn set_brightness(&mut self, level: u8) -> Result<(), String> {
        let level = level.min(100);
        let absolute = 255 - (level as u16 * 255 / 100);
        self.send_command(Command::SetBrightness, absolute, 0, 0, 0)
    }

    pub fn set_orientation(&mut self, orientation: Orientation) -> Result<(), String> {
        self.orientation = orientation;
        let (w, h) = (self.width(), self.height());
        let mut frame = [0u8; 16];
        frame[5] = Command::SetOrientation as u8;
        frame[6] = orientation as u8 + 100;
        frame[7] = (w >> 8) as u8;
        frame[8] = (w & 255) as u8;
        frame[9] = (h >> 8) as u8;
        frame[10] = (h & 255) as u8;
        self.write(&frame)
    }

    /// Push RGB565-LE pixels into the rectangle at (`x`, `y`).
    ///
    /// `data` must hold exactly `w * h` pixels, row-major.
    pub fn display_bitmap(
        &mut self,
        x: u16,
        y: u16,
        w: u16,
        h: u16,
        data: &[u8],
    ) -> Result<(), String> {
        if w == 0 || h == 0 {
            return Ok(());
        }
        let expected = w as usize * h as usize * 2;
        if data.len() != expected {
            return Err(format!(
                "bitmap is {} bytes but {w}x{h} needs {expected}",
                data.len()
            ));
        }
        self.send_command(Command::DisplayBitmap, x, y, x + w - 1, y + h - 1)?;
        // Chunked to match the reference implementation's pacing; hardware
        // flow control does the actual throttling.
        for chunk in data.chunks(self.display_width as usize * 8) {
            self.write(chunk)?;
        }
        Ok(())
    }
}

/// Read until the buffer is full or the port times out; returns bytes read.
fn read_exact_or_timeout(port: &mut dyn SerialPort, buf: &mut [u8]) -> usize {
    let mut n = 0;
    while n < buf.len() {
        match port.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(_) => break,
        }
    }
    n
}

/// Find the panel's tty by walking sysfs, so we don't need libudev.
pub fn detect_port() -> Option<String> {
    let mut fallback = None;
    for entry in std::fs::read_dir("/sys/class/tty").ok()? {
        let entry = entry.ok()?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with("ttyACM") && !name.starts_with("ttyUSB") {
            continue;
        }
        let Some(usb) = usb_parent(&entry.path().join("device")) else {
            continue;
        };
        let dev = format!("/dev/{name}");
        // Serial number is the stronger signal: rebadged panels reuse the
        // generic WCH vid/pid, and other WCH adapters share it too.
        if read_attr(&usb, "serial").as_deref() == Some(SERIAL) {
            return Some(dev);
        }
        let vid = read_hex(&usb, "idVendor");
        let pid = read_hex(&usb, "idProduct");
        if vid == Some(VID) && pid == Some(PID) && fallback.is_none() {
            fallback = Some(dev);
        }
    }
    fallback
}

/// Walk up from a tty's device link until we hit the USB device node that
/// carries the descriptors.
fn usb_parent(link: &Path) -> Option<PathBuf> {
    let mut dir = std::fs::canonicalize(link).ok()?;
    for _ in 0..6 {
        if dir.join("idVendor").is_file() {
            return Some(dir);
        }
        dir = dir.parent()?.to_path_buf();
    }
    None
}

fn read_attr(dir: &Path, name: &str) -> Option<String> {
    std::fs::read_to_string(dir.join(name))
        .ok()
        .map(|s| s.trim().to_string())
}

fn read_hex(dir: &Path, name: &str) -> Option<u16> {
    u16::from_str_radix(&read_attr(dir, name)?, 16).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reimplementation of the frame packing, kept separate from `Device` so
    /// it can be checked without a serial port.
    fn pack(cmd: u8, x: u16, y: u16, ex: u16, ey: u16) -> [u8; 6] {
        [
            (x >> 2) as u8,
            (((x & 3) << 6) | (y >> 4)) as u8,
            (((y & 15) << 4) | (ex >> 6)) as u8,
            (((ex & 63) << 2) | (ey >> 8)) as u8,
            (ey & 255) as u8,
            cmd,
        ]
    }

    /// Expected values produced by the reference Python implementation.
    #[test]
    fn command_frames_match_reference() {
        assert_eq!(
            pack(Command::DisplayBitmap as u8, 0, 0, 319, 479),
            [0, 0, 4, 253, 223, 197]
        );
        assert_eq!(pack(Command::Clear as u8, 0, 0, 0, 0), [0, 0, 0, 0, 0, 102]);
        assert_eq!(
            pack(Command::SetBrightness as u8, 102, 0, 0, 0),
            [25, 128, 0, 0, 0, 110]
        );
        assert_eq!(
            pack(Command::DisplayBitmap as u8, 137, 261, 200, 300),
            [34, 80, 83, 33, 44, 197]
        );
    }

    #[test]
    fn brightness_is_inverted() {
        // The panel takes 0 as brightest, so 100% must map to 0.
        let level = |pct: u16| 255 - (pct * 255 / 100);
        assert_eq!(level(100), 0);
        assert_eq!(level(0), 255);
        assert_eq!(level(50), 128);
    }

    #[test]
    fn sub_revision_sizes() {
        assert_eq!(SubRevision::Turing35.size(), (320, 480));
        assert_eq!(SubRevision::UsbMonitor5.size(), (480, 800));
        assert_eq!(SubRevision::UsbMonitor7.size(), (600, 1024));
    }
}
