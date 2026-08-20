// SPDX-License-Identifier: GPL-3.0-or-later
//! Minimal use of the library: draw a bar and some text, then flush.
//!
//! Run with: cargo run -p turing-lcd --example demo

use turing_lcd::canvas::Rect;
use turing_lcd::widgets::progress_bar;
use turing_lcd::{Align, Display, Font, Orientation, VAlign};

fn main() -> Result<(), String> {
    let mut display = Display::open(None, Orientation::Portrait)?;
    display.device_mut().set_brightness(60)?;
    display.device_mut().screen_on()?;
    println!(
        "{} — {:?} — {}x{}",
        display.device().path(),
        display.device().sub_revision(),
        display.width(),
        display.height()
    );

    let mut font = Font::from_bytes(include_bytes!("../../assets/RobotoMono-Bold.ttf"))?;
    let width = display.width();

    for step in 0..=20 {
        let fraction = step as f32 / 20.0;
        let canvas = display.canvas();
        canvas.clear([0x0d, 0x11, 0x17]);
        font.draw(
            canvas,
            "turing-lcd",
            (width / 2) as i32,
            40,
            24,
            [0xe6, 0xed, 0xf3],
            Align::Center,
            VAlign::Top,
        );
        font.draw(
            canvas,
            &format!("{:.0}%", fraction * 100.0),
            (width / 2) as i32,
            90,
            32,
            [0x58, 0xa6, 0xff],
            Align::Center,
            VAlign::Top,
        );
        progress_bar(
            canvas,
            Rect::new(20, 150, width - 40, 24),
            fraction,
            [0x58, 0xa6, 0xff],
            [0x21, 0x26, 0x2d],
            Some([0x30, 0x36, 0x3d]),
        );

        // Only the pixels that changed since the previous frame are sent.
        let stats = display.flush()?;
        println!("frame {step}: {} bytes in {} rect(s)", stats.bytes, stats.rects);
        std::thread::sleep(std::time::Duration::from_millis(150));
    }
    Ok(())
}
