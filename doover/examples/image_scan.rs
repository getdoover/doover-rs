//! A PLC-style scan on the platform interface's process image, and how fresh
//! the image is at each scan. Run on a device with a platform interface:
//!
//!     image_scan [uri] [scan_hz] [seconds] [ai]
//!
//! Defaults: http://127.0.0.1:50053, 50 Hz, 10 s, AI 0. Also times
//! `fetch_image` and `fetch_ai` calls for comparison.

use std::time::{Duration, Instant};

use doover::docker::PlatformClient;

fn pct(v: &mut [f64], q: f64) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[((v.len() as f64 * q) as usize).min(v.len() - 1)]
}

#[tokio::main]
async fn main() -> doover::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let uri = args.get(1).cloned().unwrap_or_else(|| "http://127.0.0.1:50053".into());
    let hz: f64 = args.get(2).and_then(|v| v.parse().ok()).unwrap_or(50.0);
    let secs: u64 = args.get(3).and_then(|v| v.parse().ok()).unwrap_or(10);
    let ai: i32 = args.get(4).and_then(|v| v.parse().ok()).unwrap_or(0);

    let plt = PlatformClient::connect(uri).await?;
    plt.wait_until_healthy(Duration::from_millis(200)).await;

    // One-off calls, for comparison with reading the watched image.
    let (mut fetch_image_ms, mut fetch_ai_ms) = (Vec::new(), Vec::new());
    for _ in 0..200 {
        let t = Instant::now();
        plt.fetch_image().await?;
        fetch_image_ms.push(t.elapsed().as_secs_f64() * 1e3);
        let t = Instant::now();
        plt.fetch_ai(ai).await?;
        fetch_ai_ms.push(t.elapsed().as_secs_f64() * 1e3);
    }

    let watch = plt.watch_image(None);
    while watch.latest().is_none() {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let mut ticker = tokio::time::interval(Duration::from_secs_f64(1.0 / hz));
    let (mut ages, mut read_us, mut scans, mut new_images) = (Vec::new(), Vec::new(), 0u64, 0u64);
    let mut last_seq = 0;
    let mut last_value = None;
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end {
        ticker.tick().await;
        let t = Instant::now();
        let latest = watch.latest().expect("image");
        let value = latest.image.good_ai(ai);
        read_us.push(t.elapsed().as_secs_f64() * 1e6);
        ages.push(latest.age().as_secs_f64() * 1e3);
        if latest.image.sequence != last_seq {
            new_images += 1;
            last_seq = latest.image.sequence;
        }
        last_value = value;
        scans += 1;
    }
    println!("fetch_image: p50 {:.2} p99 {:.2} ms", pct(&mut fetch_image_ms, 0.5), pct(&mut fetch_image_ms, 0.99));
    println!("fetch_ai:    p50 {:.2} p99 {:.2} ms", pct(&mut fetch_ai_ms, 0.5), pct(&mut fetch_ai_ms, 0.99));
    println!(
        "scan at {hz} Hz for {secs} s: {scans} scans, {new_images} new images ({:.1}/s); \
         reading the image p50 {:.1} p99 {:.1} us; image age at scan p50 {:.1} p99 {:.1} max {:.1} ms; AI{ai} {last_value:?}",
        new_images as f64 / secs as f64,
        pct(&mut read_us, 0.5), pct(&mut read_us, 0.99),
        pct(&mut ages, 0.5), pct(&mut ages, 0.99), pct(&mut ages, 1.0),
    );
    Ok(())
}
