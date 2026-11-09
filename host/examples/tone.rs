//! Plays a 440 Hz tone into the first hostmic microphone for ten seconds, with the reference
//! loop, then prints what the loop did.
//!
//! ```text
//! cargo run -p hostmic --example tone
//! ```

use std::{f64::consts::TAU, thread, time::Duration};

use hostmic::{FormatRange, Host, Player, SAMPLE_TYPES_ALL};

fn main() -> Result<(), hostmic::Error> {
    let host = Host::attach(
        &hostmic::first_interface()?,
        // Whatever the driver streams: the player converts to it.
        FormatRange {
            min_rate: 8_000,
            max_rate: 384_000,
            min_channels: 1,
            max_channels: 32,
            sample_types: SAMPLE_TYPES_ALL,
        },
    )?;
    // The callback runs on the real-time thread: no allocation, lock or I/O in it.
    let player = Player::start(host, |block| {
        let frames_per_second = block.format().rate.max(1);
        let rate = f64::from(frames_per_second);
        let channels = block.channels().max(1);
        let start = block.position();
        for (index, frame) in block.samples_mut().chunks_mut(channels).enumerate() {
            #[allow(
                clippy::cast_precision_loss,
                clippy::cast_possible_truncation,
                reason = "the frame number modulo the rate is far below 2^52"
            )]
            let value = {
                let n = (start + index as u64) % u64::from(frames_per_second);
                (0.25 * (TAU * 440.0 * n as f64 / rate).sin()) as f32
            };
            frame.fill(value);
        }
    })?;
    println!("playing: record \"Microphone (hostmic)\" to hear it");
    thread::sleep(Duration::from_secs(10));
    println!("{:?}", player.stats());
    player.stop()?.detach()
}
