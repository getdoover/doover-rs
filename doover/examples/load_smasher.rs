//! Native-Rust load generator for the device agent — the client counterpart
//! to `stress/bench_rps.py`. Because `DeviceAgentClient` multiplexes over one
//! persistent HTTP/2 channel, a single one can drive far more than the Python
//! client's ceiling, letting you measure the agent's *real* throughput limit
//! (and, paired with an external CPU sampler, its cost per RPS).
//!
//! Usage:
//!   cargo run --release --example load_smasher -- \
//!       --uri http://127.0.0.1:50051 --concurrency 64 --duration 20 --channels 8
//!   # paced instead of saturate:
//!   cargo run --release --example load_smasher -- --rps 100 --duration 30
//!
//! Flags: --uri --concurrency --rps --duration --channels --op {aggregate,message}

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use doover::error::Result;
use doover::{AggregateOptions, DeviceAgentClient};
use serde_json::json;
use tokio::sync::Mutex;

struct Args {
    uri: String,
    concurrency: usize,
    rps: Option<f64>,
    duration: f64,
    channels: usize,
    op: String,
    /// If set, just fetch this channel's aggregate and print it, then exit.
    read: Option<String>,
    /// `--set <channel> --data <json>`: write one aggregate update and exit.
    set: Option<String>,
    data: Option<String>,
    /// `--count-oneshots <channel> --secs <n>`: subscribe and count
    /// OneShotMessage events for n seconds (live-mode verification).
    count_oneshots: Option<String>,
    secs: f64,
    /// If >0, open this many ChannelEventSubscription streams and hold them
    /// (consuming events) instead of generating load. Used to verify the
    /// agent's graceful shutdown with open streams — SIGINT the agent and it
    /// must still exit promptly.
    subscribe: usize,
}

fn parse_args() -> Args {
    let mut a = Args {
        uri: "http://127.0.0.1:50051".into(),
        concurrency: 64,
        rps: None,
        duration: 20.0,
        channels: 8,
        op: "aggregate".into(),
        read: None,
        set: None,
        data: None,
        count_oneshots: None,
        secs: 5.0,
        subscribe: 0,
    };
    let argv: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < argv.len() {
        let get = |i: &mut usize| {
            *i += 1;
            argv.get(*i).cloned().unwrap_or_default()
        };
        match argv[i].as_str() {
            "--uri" => a.uri = get(&mut i),
            "--concurrency" => a.concurrency = get(&mut i).parse().unwrap_or(a.concurrency),
            "--rps" => a.rps = get(&mut i).parse().ok(),
            "--duration" => a.duration = get(&mut i).parse().unwrap_or(a.duration),
            "--channels" => a.channels = get(&mut i).parse().unwrap_or(a.channels),
            "--op" => a.op = get(&mut i),
            "--read" => a.read = Some(get(&mut i)),
            "--set" => a.set = Some(get(&mut i)),
            "--data" => a.data = Some(get(&mut i)),
            "--count-oneshots" => a.count_oneshots = Some(get(&mut i)),
            "--secs" => a.secs = get(&mut i).parse().unwrap_or(5.0),
            "--subscribe" => a.subscribe = get(&mut i).parse().unwrap_or(0),
            other => eprintln!("ignoring unknown arg {other}"),
        }
        i += 1;
    }
    if !a.uri.contains("://") {
        a.uri = format!("http://{}", a.uri);
    }
    a
}

async fn one_call(client: &DeviceAgentClient, op: &str, channel: &str, i: u64) -> Result<()> {
    let payload = json!({
        "seq": i,
        "value": (i % 1000) as f64 * 0.5,
        "flag": i & 1 == 1,
    });
    match op {
        "message" => {
            client.create_message(channel, &payload).await?;
        }
        _ => {
            client
                .update_channel_aggregate(
                    channel,
                    &payload,
                    &AggregateOptions { max_age_secs: 0.0, ..Default::default() },
                )
                .await?;
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = parse_args();
    let client = DeviceAgentClient::connect(args.uri.clone()).await?;

    // Read mode: fetch one channel aggregate and print it (round-trip proof).
    if let Some(ch) = &args.read {
        match client.fetch_channel_aggregate(ch).await? {
            Some(data) => println!("{}", serde_json::to_string_pretty(&data).unwrap_or_default()),
            None => println!("(channel {ch} not found)"),
        }
        return Ok(());
    }

    // Set mode: write one aggregate update (e.g. inject a dv-ui-sub claim).
    if let Some(ch) = &args.set {
        let data: serde_json::Value = serde_json::from_str(args.data.as_deref().unwrap_or("{}"))?;
        client
            .update_channel_aggregate(ch, &data, &Default::default())
            .await?;
        println!("set {ch} = {}", serde_json::to_string(&data).unwrap_or_default());
        return Ok(());
    }

    // Count-oneshots mode: subscribe and count OneShotMessage events for --secs.
    if let Some(ch) = &args.count_oneshots {
        use futures_util::StreamExt;
        let mut stream = client.subscribe_events(ch).await?;
        let deadline = Instant::now() + Duration::from_secs_f64(args.secs);
        let mut count = 0u64;
        let start = Instant::now();
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            match tokio::time::timeout(remaining, stream.next()).await {
                Ok(Some(Ok(ev))) => {
                    if ev.event_name == "OneShotMessage" {
                        count += 1;
                    }
                }
                Ok(Some(Err(_))) | Ok(None) => break,
                Err(_) => break, // timed out = window elapsed
            }
        }
        let secs = start.elapsed().as_secs_f64();
        println!(
            "{}",
            serde_json::json!({"channel": ch, "oneshots": count, "secs": (secs*100.0).round()/100.0, "rate_hz": (count as f64/secs*10.0).round()/10.0})
        );
        return Ok(());
    }

    let channels: Vec<String> = (0..args.channels).map(|i| format!("smash_ch{i}")).collect();

    // Subscribe mode: open N event streams and hold them until Ctrl-C. This is
    // the on-device check for the graceful-shutdown fix — while this is
    // running, SIGINT the agent and confirm it exits promptly (not SIGKILLed).
    if args.subscribe > 0 {
        use futures_util::StreamExt;
        let mut handles = Vec::new();
        for i in 0..args.subscribe {
            let client = client.clone();
            let ch = format!("smash_sub{i}");
            // ensure the channel exists so the subscribe has something to sync
            let _ = one_call(&client, "aggregate", &ch, 0).await;
            handles.push(tokio::spawn(async move {
                match client.subscribe_events(&ch).await {
                    Ok(mut stream) => {
                        while let Some(ev) = stream.next().await {
                            if ev.is_err() {
                                break;
                            }
                        }
                    }
                    Err(e) => eprintln!("subscribe {ch} failed: {e}"),
                }
            }));
        }
        eprintln!(">>> holding {} event subscriptions; SIGINT the agent to test shutdown", args.subscribe);
        let _ = tokio::signal::ctrl_c().await;
        for h in handles {
            h.abort();
        }
        return Ok(());
    }

    // warmup
    for ch in &channels {
        let _ = one_call(&client, &args.op, ch, 0).await;
    }

    let counter = Arc::new(AtomicU64::new(0));
    let ok = Arc::new(AtomicU64::new(0));
    let errs = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let latencies = Arc::new(Mutex::new(Vec::<f64>::with_capacity(1_000_000)));

    let start = Instant::now();
    let mode = if args.rps.is_some() { "paced" } else { "saturate" };
    eprintln!(
        "# load_smasher mode={mode} op={} concurrency={} channels={} duration={}s uri={}",
        args.op, args.concurrency, args.channels, args.duration, args.uri
    );

    // stopper
    {
        let stop = stop.clone();
        let dur = args.duration;
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs_f64(dur)).await;
            stop.store(true, Ordering::Relaxed);
        });
    }

    let mut workers = Vec::new();
    let per_worker_interval = args.rps.map(|rps| Duration::from_secs_f64(args.concurrency as f64 / rps));
    for _ in 0..args.concurrency {
        let client = client.clone();
        let channels = channels.clone();
        let counter = counter.clone();
        let ok = ok.clone();
        let errs = errs.clone();
        let stop = stop.clone();
        let latencies = latencies.clone();
        let op = args.op.clone();
        let interval = per_worker_interval;
        workers.push(tokio::spawn(async move {
            let mut local_lat: Vec<f64> = Vec::new();
            let mut next = Instant::now();
            while !stop.load(Ordering::Relaxed) {
                if let Some(iv) = interval {
                    let now = Instant::now();
                    if now < next {
                        tokio::time::sleep(next - now).await;
                    }
                    next += iv;
                }
                let i = counter.fetch_add(1, Ordering::Relaxed);
                let ch = &channels[(i as usize) % channels.len()];
                let t0 = Instant::now();
                match one_call(&client, &op, ch, i).await {
                    Ok(()) => {
                        local_lat.push(t0.elapsed().as_secs_f64() * 1000.0);
                        ok.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(_) => {
                        errs.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            latencies.lock().await.extend(local_lat);
        }));
    }

    for w in workers {
        let _ = w.await;
    }
    let elapsed = start.elapsed().as_secs_f64();
    let total = ok.load(Ordering::Relaxed) + errs.load(Ordering::Relaxed);
    let mut lat = latencies.lock().await;
    lat.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let pctl = |p: f64| -> f64 {
        if lat.is_empty() {
            return 0.0;
        }
        let k = (((p / 100.0) * (lat.len() - 1) as f64).round() as usize).min(lat.len() - 1);
        lat[k]
    };
    println!(
        "{}",
        json!({
            "mode": mode,
            "op": args.op,
            "concurrency": args.concurrency,
            "duration_s": (elapsed * 100.0).round() / 100.0,
            "requests": total,
            "ok": ok.load(Ordering::Relaxed),
            "errors": errs.load(Ordering::Relaxed),
            "achieved_rps": (total as f64 / elapsed * 10.0).round() / 10.0,
            "p50_ms": (pctl(50.0) * 100.0).round() / 100.0,
            "p95_ms": (pctl(95.0) * 100.0).round() / 100.0,
            "p99_ms": (pctl(99.0) * 100.0).round() / 100.0,
        })
    );
    eprintln!(
        ">>> rust client: {:.0} rps ({} reqs in {:.1}s), p95 {:.2}ms, errors {}",
        total as f64 / elapsed, total, elapsed, pctl(95.0), errs.load(Ordering::Relaxed)
    );
    Ok(())
}
