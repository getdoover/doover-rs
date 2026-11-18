//! How evenly `subscribeImage` delivers: every image on the raw stream, with
//! the gap between messages. Run on a device with a platform interface:
//!
//!     image_subscribe [uri] [seconds]
//!
//! Defaults: http://127.0.0.1:50053, 10 s.

use std::time::{Duration, Instant};

use doover::docker::PlatformClient;
use futures_util::StreamExt;

fn pct(v: &mut [f64], q: f64) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[((v.len() as f64 * q) as usize).min(v.len() - 1)]
}

#[tokio::main]
async fn main() -> doover::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let uri = args
        .get(1)
        .cloned()
        .unwrap_or_else(|| "http://127.0.0.1:50053".into());
    let secs: u64 = args.get(2).and_then(|v| v.parse().ok()).unwrap_or(10);

    let client = PlatformClient::connect(uri).await?;
    let mut stream = client.subscribe_image(None).await?;
    stream.next().await; // the current image, sent at once
    let end = Instant::now() + Duration::from_secs(secs);
    let (mut last, mut first_seq, mut last_seq) = (Instant::now(), None, 0u64);
    let (mut n, mut seq_jumps) = (0u64, 0u64);
    let mut gaps = Vec::new();
    while let Ok(Some(item)) = tokio::time::timeout_at(end.into(), stream.next()).await {
        let image = item?;
        let now = Instant::now();
        if n > 0 {
            gaps.push((now - last).as_secs_f64() * 1000.0);
            if image.sequence != last_seq + 1 {
                seq_jumps += 1;
            }
        }
        first_seq.get_or_insert(image.sequence);
        last_seq = image.sequence;
        last = now;
        n += 1;
    }
    let within =
        |gaps: &[f64], lo: f64, hi: f64| gaps.iter().filter(|g| **g >= lo && **g < hi).count();
    let (even, short, long) = (
        within(&gaps, 17.0, 23.0),
        within(&gaps, 0.0, 10.0),
        within(&gaps, 30.0, f64::MAX),
    );
    println!(
        "{n} images in {secs} s ({:.1}/s), sequence span {}, jumps {seq_jumps}",
        n as f64 / secs as f64,
        last_seq - first_seq.unwrap_or(last_seq)
    );
    println!(
        "gap ms: p1 {:.1} p50 {:.1} p99 {:.1} max {:.1}; 17-23 ms {} of {}; <10 ms {}; >30 ms {}",
        pct(&mut gaps, 0.01),
        pct(&mut gaps, 0.5),
        pct(&mut gaps, 0.99),
        pct(&mut gaps, 1.0),
        even,
        gaps.len(),
        short,
        long,
    );
    Ok(())
}
