# doover-rs

A client library for writing **Doover device applications in Rust** — the Rust
counterpart to [`pydoover`](../pydoover). Apps talk to the local **Doover
Device Agent** (DDA) over gRPC to read and write channels (aggregates,
messages, events); this workspace wraps that contract in an ergonomic async API
plus an `Application` runtime that mirrors pydoover's `run_app` lifecycle.

Status: **early draft.** The core DDA client and app runtime work end-to-end
against a real agent; the higher-level declarative frameworks (config/tags/UI
codegen) and the hardware sidecar clients are not ported yet — see the roadmap.

## Layout

- **`doover-proto/`** — tonic/prost codegen for `device_agent.proto`, vendored
  verbatim from pydoover (`protos/device_agent.proto`). Client stubs only.
  Re-vendor when the proto changes. No protobuf toolchain needed to build
  (uses `protoc-bin-vendored` unless `PROTOC` is set).
- **`doover/`** — the client library.
  - `client.rs` — `DeviceAgentClient`: `update_channel_aggregate`,
    `fetch_channel_aggregate`, `create_message`, `send_one_shot_message`,
    `subscribe_events`, `test_comms`. Speaks the lossless **`data_json`**
    encoding (requests `WIRE_FORMAT_JSON_ONLY`, so the agent never builds the
    lossy protobuf `Struct`), maps `ResponseHeader` to typed errors
    (404 → `NotFound`), and ports pydoover's `validate_payload`.
  - `application.rs` — the `Application` trait (`setup` + `main_loop`) and the
    drift-corrected `run_app` loop, with `AppContext` helpers
    (`update_channel_aggregate`, `create_message`, `set_tag`/`set_tags`).
  - `config.rs` — dynamic config from `CONFIG_FP`.
  - `events.rs` — typed `Event` accessors over the event `data_json`.
  - `platform.rs` — `PlatformClient` for the platform-interface sidecar
    (`platform_iface.getAI` → `fetch_ai`); the rest of the DI/DO/AO/power
    surface is future work.
  - `examples/level_sensor.rs` — a faithful port of the Python
    `analog-level-sensor`: reads its AI pin from the platform interface and
    publishes level/percentage/volume tags (verified on a CM4 against the live
    platform interface).
  - `examples/load_smasher.rs` — a native-Rust load generator (one persistent
    HTTP/2 channel, many concurrent requests) for measuring the agent's real
    throughput ceiling.

## Quick start

```rust
use doover::{Application, AppContext, run_app};
use doover::error::Result;
use serde_json::json;

struct MyApp { level: f64 }

#[doover::async_trait]
impl Application for MyApp {
    async fn main_loop(&mut self, ctx: &AppContext) -> Result<()> {
        self.level += 1.0;
        ctx.set_tag("level", json!(self.level)).await?;
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    run_app(MyApp { level: 0.0 }).await
}
```

Env/args (pydoover parity): `DDA_URI` (default `127.0.0.1:50051`), `APP_KEY`,
`CONFIG_FP`.

## Build & run

```sh
cargo build --release
cargo test                       # payload-validation unit tests + doctest
# run the example app against a local agent on :50051
DDA_URI=127.0.0.1:50051 APP_KEY=level_sensor cargo run --release --example level_sensor
# saturate the agent to find its ceiling
cargo run --release --example load_smasher -- --uri http://127.0.0.1:50051 --concurrency 64 --duration 20
```

## Why Rust clients

Measured against a local agent (Apple Silicon, release builds):

| Driver | Sustained RPS to the agent | Notes |
|---|---|---|
| pydoover-style client (channel per call) | ~100–6 k | per-call channel setup dominates; the historical ~100 RPS ceiling is a single-inflight client |
| Python asyncio client (persistent channel, 32 conc.) | ~6 k | the Python load generator saturates before the Rust agent does |
| **doover-rs `load_smasher` (1 persistent channel)** | **agent-bound** | one client thread multiplexes thousands of concurrent requests; the agent, not the client, becomes the limit |

At a sustained 100 RPS the **Rust agent** costs ~6 % of one core / ~16 MB RSS
(vs the Python agent's ~15 % / ~69 MB). A Rust *client* adds a similarly small
footprint, which matters on constrained CM4-class devices running many apps.

## Roadmap (not yet ported)

Prioritized, mirroring the pydoover surface:

1. ~~**`deployment_config` bootstrap**~~ — DONE: `run_app` fetches the
   `deployment_config` aggregate and injects `applications.<app_key>` when no
   `CONFIG_FP` is set (verified on a CM4 against a live deployment).
2. **Event subscribe reconnect loop** — auto-reconnect with backoff + synthetic
   `ChannelSync` on (re)subscribe, matching `DeviceAgentInterface`. (The
   `dv-ui-sub` watcher used by live mode already does a basic version.)
3. **Declarative `Schema` / `Tags` / `UI`** via derive macros → JSON-Schema
   export for `doover_config.json`. Tag *runtime* features are done:
   `set_tag`/`set_tags` write `tag_values`, and **live mode** streams
   `live_tags()` as one-shots at the loop rate whenever a user has the tag
   open (`dv-ui-sub` observation, `is_live_tag_open`/`is_being_observed`) —
   verified on a CM4. Still to port: `log_on` triggers, the declarative macros.
4. **`platform_iface` + `modbus_iface` clients** — `fetch_ai` is done; still to
   port: DI/DO/AO, pulse counters, power/shutdown/location, and Modbus.
5. **RPC + notifications** (`dv-rpc`, `send_notification`).
6. **Healthcheck HTTP server** on `HEALTHCHECK_PORT` (49200).

The proto contract, error taxonomy, `data_json` codec, payload validation, and
the loop lifecycle — the parts that are easy to get subtly wrong — are already
in place, so the remaining work is additive.
