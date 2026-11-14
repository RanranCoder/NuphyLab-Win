// Standalone loopback validation: initializes WASAPI loopback on the default
// render endpoint exactly the way audio.rs does and prints sample stats for a
// few seconds. Needs no keyboard and no server: play anything while it runs.
use std::collections::VecDeque;
use std::time::{Duration, Instant};
use wasapi::{get_default_device, initialize_mta, Direction, ShareMode};

fn main() -> Result<(), String> {
    initialize_mta().ok().map_err(|e| format!("COM init: {e:?}"))?;
    let device = get_default_device(&Direction::Render).map_err(|e| e.to_string())?;
    println!("render device: {}", device.get_friendlyname().map_err(|e| e.to_string())?);
    let client = device.get_iaudioclient().map_err(|e| e.to_string())?;
    let mix = client.get_mixformat().map_err(|e| e.to_string())?;
    println!(
        "mix format: {} Hz, {} ch, {} bit, subformat {}",
        mix.get_samplespersec(),
        mix.get_nchannels(),
        mix.get_bitspersample(),
        mix.get_subformat().map(|s| s.to_string()).unwrap_or_default()
    );
    let rate = mix.get_samplespersec();
    let blockalign = mix.get_blockalign() as usize;
    let (_, min_period) = client.get_periods().map_err(|e| e.to_string())?;
    let mut client = client;
    // Render device + Capture direction = AUDCLNT_STREAMFLAGS_LOOPBACK.
    client
        .initialize_client(&mix, min_period, &Direction::Capture, &ShareMode::Shared, false)
        .map_err(|e| format!("initialize: {e}"))?;
    let _event = client.set_get_eventhandle().map_err(|e| format!("event handle: {e}"))?;
    let capture = client.get_audiocaptureclient().map_err(|e| format!("capture client: {e}"))?;
    client.start_stream().map_err(|e| format!("start: {e}"))?;
    println!("stream started; observing for 6s (play something!)...");
    let mut queue = VecDeque::new();
    let deadline = Instant::now() + Duration::from_secs(6);
    let mut frames = 0usize;
    let mut peak = 0.0f32;
    while Instant::now() < deadline {
        capture.read_from_device_to_deque(&mut queue).map_err(|e| e.to_string())?;
        if queue.is_empty() {
            std::thread::sleep(Duration::from_millis(4));
            continue;
        }
        let usable = queue.len() / blockalign * blockalign;
        let bytes: Vec<u8> = queue.drain(..usable).collect();
        for frame in bytes.chunks_exact(blockalign) {
            if frame.len() >= 4 {
                peak = peak.max(f32::from_le_bytes([frame[0], frame[1], frame[2], frame[3]]).abs());
            }
            frames += 1;
        }
    }
    let _ = client.stop_stream();
    println!("captured {frames} frames ({:.2}s at {rate} Hz), peak {peak:.4}", frames as f64 / rate as f64);
    if peak > 0.0 {
        println!("LOOPBACK OK: samples carry signal");
    } else {
        println!("loopback works but captured silence (nothing was playing)");
    }
    Ok(())
}
