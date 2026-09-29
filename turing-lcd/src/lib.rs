// SPDX-License-Identifier: GPL-3.0-or-later
//! Driver and drawing library for Turing Smart Screen rev. A / UsbMonitor
//! USB-C IPS panels (USB 1a86:5722, serial `USB35INCHIPSV2`).
//!
//! Rust port of the rev. A protocol from
//! <https://github.com/mathoudebine/turing-smart-screen-python> (GPL-3.0).

pub mod canvas;
pub mod device;
pub mod display;
pub mod text;
pub mod widgets;

pub use canvas::{Canvas, Image, Rect, Rgb};
pub use device::{detect_port, Device, Orientation, SubRevision};
pub use display::{Display, FlushStats};
pub use text::{Align, Font, VAlign};
