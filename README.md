# doover-rs

A client library for writing **Doover device applications in Rust** — the Rust
counterpart to [`pydoover`](../pydoover). Apps talk to the local **Doover
Device Agent** (DDA) over gRPC to read and write channels (aggregates,
messages, events); this workspace wraps that contract in an ergonomic async API
plus an `Application` runtime that mirrors pydoover's `run_app` lifecycle.

Status: the core DDA client, the declarative config/tags/UI framework with
byte-exact `doover_config.json` export, the app runtime (ui_state publishing,
ui_cmds command dispatch, RPC, notifications), the hardware sidecar clients,
and the **processor client** (AWS Lambda + HTTP data API, see
[Processors](#processors-aws-lambda)) are ported and tested; see the roadmap
for what's left.

## Layout

- **`doover-proto/`** — tonic/prost codegen for `device_agent.proto`, vendored
  verbatim from pydoover (`protos/device_agent.proto`). Client stubs only.
  Re-vendor when the proto changes. No protobuf toolchain needed to build
  (uses `protoc-bin-vendored` unless `PROTOC` is set).
- **`doover/`** — the client library.
  - `docker/device_agent.rs` — `DeviceAgentClient`: aggregates, messages,
    one-shots, event subscriptions. Speaks the lossless **`data_json`**
    encoding (requests `WIRE_FORMAT_JSON_ONLY`, so the agent never builds the
    lossy protobuf `Struct`), maps `ResponseHeader` to typed errors
    (404 → `NotFound`), and ports pydoover's `validate_payload`.
  - `docker/application.rs` — the `Application` trait (associated
    `Config`/`Tags`/`Ui` types + `setup`/`main_loop`/`on_ui_command`) and the
    drift-corrected `doover::run` loop with the built-in `export` subcommand.
  - `config/`, `tags/`, `ui/` — the declarative framework (derive-backed
    schema emission, typed `Tag<T>` handles, UI element structs and the
    `ui_state`/`ui_cmds` runtime).
  - `channel_backend.rs` + `rpc.rs` — the transport-agnostic channel trait
    and the `dv-rpc` RPC manager built on it.
  - `api/` (feature `cloud-api`) — the async HTTP `DataClient` for
    data.doover.com, also a `ChannelBackend`.
  - `processor/` (feature `processor`) — the AWS Lambda processor runtime
    (`Processor` trait, dispatch pipeline, `TagsManagerProcessor` port,
    invocation summaries, `run_processor` / `handle_event_local`).
  - `testing.rs` (feature `testing`) — in-memory `MockBackend` recorder.
  - `docker/platform.rs` / `docker/modbus.rs` — the hardware sidecar clients.
  - `examples/level_sensor.rs` — a faithful declarative port of the Python
    `analog-level-sensor` (typed config incl. volume curve, live tags,
    runtime UI promotion), exporting its own byte-identical
    `doover_config.json`.
  - `examples/load_smasher.rs` — a native-Rust load generator (one persistent
    HTTP/2 channel, many concurrent requests) for measuring the agent's real
    throughput ceiling.

## Quick start

Declare typed tags and UI (and optionally a `#[derive(Config)]` schema), and
the runtime loads the deployment config, attaches the tags, builds and
publishes the UI, and dispatches user commands back to the app:

```rust
use doover::error::Result;
use doover::tags::Tag;
use doover::ui::{NumericVariable, Switch, UiBuild};
use doover::{AppContext, Application, Tags, Ui, UiCommand};

#[derive(Tags)]
struct MyTags {
    #[tag(live, default = None)]
    level: Tag<f64>,
}

#[derive(Ui)]
struct MyUi {
    level: NumericVariable,
    pump: Switch,
}

impl UiBuild for MyUi {
    type Tags = MyTags;

    fn build(tags: &MyTags) -> Self {
        Self {
            level: NumericVariable::new("Level").units("%").value(&tags.level),
            pump: Switch::new("Pump"),
        }
    }
}

struct MyApp { tags: MyTags, ui: MyUi, reading: f64 }

#[doover::async_trait]
impl Application for MyApp {
    type Config = (); // or a #[derive(Config)] struct
    type Tags = MyTags;
    type Ui = MyUi;

    fn create(_config: (), tags: MyTags, ui: MyUi) -> Self {
        Self { tags, ui, reading: 0.0 }
    }

    fn ui(&self) -> Option<&MyUi> { Some(&self.ui) }
    fn ui_mut(&mut self) -> Option<&mut MyUi> { Some(&mut self.ui) }

    async fn main_loop(&mut self, _ctx: &AppContext) -> Result<()> {
        self.reading += 1.0;
        self.tags.level.set(self.reading).await
    }

    async fn on_ui_command(&mut self, _ctx: &AppContext, cmd: &UiCommand) -> Result<()> {
        if cmd.is(&self.ui.pump) {
            tracing::info!("pump set to {:?}", cmd.value_as::<bool>());
        }
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    doover::run::<MyApp>().await
}
```

Every app binary gets a built-in `export` subcommand that writes the config +
UI JSON schemas without connecting to an agent (byte-identical to pydoover's
export, merge-preserving other keys):

```sh
my-app export [doover_config.json] [--app-name NAME]
```

Env/args (pydoover parity): `DDA_URI` (default `127.0.0.1:50051`), `APP_KEY`,
`CONFIG_FP`, `PLT_URI`, `MODBUS_URI`, `HEALTHCHECK_PORT`, `REMOTE_DEV`.

## Processors (AWS Lambda)

With the `processor` feature, the same crate builds **event-driven cloud
apps**: one Lambda invocation handles one event (message create, aggregate
update, deployment, schedule, ingestion endpoint, manual invoke) over HTTP
against `data.doover.com` — the port of `pydoover.processor`. The runner
upgrades the event's minimal JWT via the subscription/schedule info endpoint
(full token + `agent_id`/`app_key` + seeded `tag_values`/`ui_cmds`/
`deployment_config`), applies pydoover's filters and `tag_values` self-loop
guard, commits buffered tags once, and fans an invocation summary out to the
`dv_proc_config.inv_targets` channels.

```toml
[dependencies]
doover = { version = "0.1", default-features = false, features = ["processor"] }
lambda_runtime = "0.13"
tokio = { version = "1", features = ["macros"] }
```

```rust
use doover::error::Result;
use doover::processor::{
    run_processor, Handled, MessageCreateEvent, Processor, ProcessorContext,
};

#[derive(Default)]
struct MyProcessor;

#[doover::async_trait]
impl Processor for MyProcessor {
    async fn on_message_create(
        &mut self,
        ctx: &ProcessorContext,
        event: &MessageCreateEvent,
    ) -> Result<Handled> {
        let count = ctx.get_tag("count").and_then(|v| v.as_i64()).unwrap_or(0);
        ctx.set_tag("count", (count + 1).into()); // committed once, at the end
        ctx.send_notification("processed a message").await?;
        Ok(Handled::Done)
    }
}

#[tokio::main]
async fn main() -> Result<(), lambda_runtime::Error> {
    run_processor::<MyProcessor>().await
}
```

Handlers you don't implement are recorded as skipped (`no_handler`) in the
invocation summary, matching pydoover. For local development and tests,
`processor::handle_event_local::<MyProcessor>(event_json)` runs one
invocation without Lambda (SNS-wrapped or raw events) and returns the
summary body; the `testing` feature adds an in-memory `MockBackend`
`ChannelBackend` for unit tests.

Deploy with [cargo-lambda](https://www.cargo-lambda.info/) on the
`provided.al2023` runtime (rustls only — no OpenSSL to cross-compile):

```sh
cargo lambda build --release --arm64
cargo lambda deploy my-processor
```

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

## Roadmap (mirroring the pydoover surface)

Done (M1 — docker runtime parity core):

1. ~~**`deployment_config` bootstrap**~~ — fetched at startup **and kept
   subscribed**: config re-injects live on channel updates
   (`Application::on_config_update`), with `AGENT_ID`/`APP_DISPLAY_NAME`
   extraction.
2. ~~**Event subscribe reconnect loop**~~ — `SubscriptionHub`: one stream task
   per channel, aggregate cache seeding (creates missing channels), synthetic
   `ChannelSync`, exponential-backoff reconnect, `wait_for_channels_sync`.
3. ~~**Application event callbacks**~~ — `ctx.subscribe(channel)` +
   `on_message_create` / `on_message_update` / `on_aggregate_update` /
   `on_oneshot_message` / `on_channel_sync` / `on_shutdown`, delivered
   between loop ticks.
4. ~~**Tags runtime**~~ (`TagsRuntime`, port of `TagsManagerDocker`) —
   buffered writes committed once per loop, `only_if_changed` diffing,
   immediate vs 15-min periodic log buckets, `max_age` 3 s/900 s by
   `is_app_open`, live-mode one-shots, tag subscriptions, `log_history`.
5. ~~**Diff engine + snowflake IDs**~~ (`doover::utils`) — byte-faithful
   `apply_diff`/`generate_diff` (all pydoover test cases ported) and the
   snowflake bit layout.
6. ~~**Healthcheck HTTP server**~~ on `HEALTHCHECK_PORT` (49200) — raw-tokio
   HTTP/1.1, 200 `OK`/503 `ERROR`, healthy after each good loop.
7. ~~**Full `RunOptions` parity**~~ — `--app-key/--dda-uri/--plt-uri/
   --modbus-uri/--config-fp/--healthcheck-port/--remote-dev/--debug` plus the
   matching env vars.
8. ~~**Message RPCs**~~ — `fetch_message`, `list_messages`, `update_message`,
   `fetch_message_attachment`, `fetch_turn_token`, `create_message_at`.

Done (M2/M3 — declarative framework + runtime wiring):

9. ~~**Declarative `Config`**~~ — `#[derive(Config)]` / `ConfigObject` /
   `ConfigEnum` with pydoover's exact JSON-Schema emission (key order,
   x-positions, int-vs-float), typed `from_value` loading, and the
   read-merge-write `doover_config.json` export (byte-identical golden tests
   against analog-level-sensor).
10. ~~**Declarative `Tags` + `UI`**~~ — `#[derive(Tags)]` typed `Tag<T>`
    handles (live flags, defaults, `$tag.app()` references) and
    `#[derive(Ui)]` element reflection over builder-style element structs
    (Numeric/Text/Boolean/DateTime variables, Button, Switch, Slider, Select,
    WarningIndicator; positions 51/52/…; the `:boolean:false:boolean:false`
    quirk replicated).
11. ~~**Application runtime**~~ — associated-type `Application` trait
    (`Config`/`Tags`/`Ui` + `create`), `doover::run::<A>()` with the built-in
    `export` subcommand, `ui_state` publishing (pydoover's double publish +
    `$config.app()` resolution, re-publish on schema change), and `ui_cmds`
    command dispatch to `on_ui_command` with pydoover's RPC-message protocol
    (aggregate write-back + log message + success/error responses).
12. ~~**RPC + notifications**~~ — `RpcManager` over the transport-agnostic
    `ChannelBackend` (`call` with timeout + acknowledged/deferred statuses,
    `fire_and_forget`, runtime handler registration) and
    `ctx.send_notification`.
13. ~~**`platform_iface` + `modbus_iface` clients**~~ and the **Rust CLI**.

Done (M6 — processor client):

14. ~~**HTTP data client**~~ (`doover::api::DataClient`, feature `cloud-api`)
    — the processor-facing subset of pydoover's `AsyncDataClient`
    (channels/aggregates/messages, processor info endpoints, connection
    pings, notifications) with gzip request compression, retry-on-5xx, the
    invoking-channel anti-recursion guard, and a `ChannelBackend` impl so
    managers run over HTTP or gRPC unchanged.
15. ~~**Processor runtime**~~ (`doover::processor`, feature `processor`) —
    the `Processor` trait + `_dispatch_invocation` pipeline (typed `op`
    events, pre/post filters, token upgrade, `tag_values` self-loop guard,
    skip semantics via the `Handled::NotImplemented` sentinel), the
    `TagsManagerProcessor` port with the full `LogMode` matrix, invocation
    summaries (camelCase `requestId`, stringified `agent_id`, `$app_id`
    substitution), `publish_ui_schema` over `replace_keys`,
    `ping_connection`, `lambda_runtime` wiring (`run_processor`) and a
    local-dev `handle_event_local` entry; plus the `testing` feature's
    in-memory `MockBackend`.

Done (declarative gap-fill):

16. ~~**Tag `log_on` triggers**~~ — Cross/Rise/Fall/Delta/AnyChange/Enter/Exit
    evaluated in the `TagsRuntime` set path (a fired trigger promotes the
    write to an immediate `tag_values` log message, deduped from the
    periodic bucket), the `#[tag(log_on(cross(15.0, deadband = 1.0)))]`
    attribute grammar, and pydoover-generated fire/no-fire fixture replays
    (`tests/compat/fixtures/log_triggers.json`).
17. ~~**Remaining UI elements**~~ — Submodule/Container/TabContainer/
    RemoteComponent (nested-children containers with pydoover's depth-first
    position counter), CameraLiveView/CameraHistory, Multiplot/Series,
    ConnectionInfo, Timestamp, and the parameter inputs
    (FloatInput/TextInput/DatetimeInput/TimeInput) — serialization
    byte-checked, key order included, against pydoover-generated fixtures
    (`tests/compat/fixtures/ui_elements.json`).

Still to port:

1. **Remote/cross-app tag references** (`config.TagRef` + `RemoteTag`).
2. **Cloud auth beyond bearer tokens** — `~/.doover` profiles, refresh-token
   / OIDC flows (`pydoover/api/auth/`); processors don't need them.
3. **Declarative processor-config authoring** — the `dv_proc_config` schema
   elements (`SubscriptionConfig`/`ScheduleConfig`/`IngestionEndpointConfig`
   /`ExtendedPermissionsConfig`); doover-rs currently only *deserializes*
   `dv_proc_config` at runtime. Also processor-side declarative UI (the
   docker `#[derive(Ui)]` machinery is not yet wired into `ui_state`
   publishing on deployment) and `dv_proc_config.log_level` application.

The proto contract, error taxonomy, `data_json` codec, payload validation, and
the loop lifecycle — the parts that are easy to get subtly wrong — are already
in place.
