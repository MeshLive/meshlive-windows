//! Explicit bounded silence through the default output; no capture or radio.
use meshlive::playback::Wasapi;
use std::{
    sync::atomic::AtomicBool,
    time::{Duration, Instant},
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::args().skip(1).collect::<Vec<_>>() != ["--silent-output-probe"] {
        return Err("Use --silent-output-probe to explicitly initialize the default output".into());
    }
    {
        let mut output = Wasapi::open_default()?;
        let cancel = AtomicBool::new(false);
        let deadline = Instant::now() + Duration::from_secs(2);
        for _ in 0..5 {
            output.write(&[0; 960], deadline, &cancel)?;
        }
    }
    println!("Native shared-output startup, 4800 silent PCM samples and cleanup completed.");
    println!("No acoustic, playout-timing, capture or peer-interoperability claim.");
    Ok(())
}
