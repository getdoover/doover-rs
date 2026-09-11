# Parity with pydoover

doover-rs is a port of [`pydoover`](https://github.com/getdoover/pydoover). This
file records **how far the port has caught up**, so the next sync knows exactly
where to start reading pydoover's history from instead of guessing at dates.

Keep it accurate — the `sync-pydoover-parity` skill reads `Synced to` below as
its diff base and rewrites this file when it finishes.

## Synced to

| | |
|---|---|
| pydoover commit | `4fa3e74` (`origin/main`) |
| pydoover version | 1.13.0 |
| Date of sync | 2026-08-12 |

The vendored protos under `doover-proto/proto/` were verified byte-identical
(comments aside) to `pydoover/protos/` at that commit. `device_agent.proto` and
`platform_iface.proto` were re-vendored in this sync (cross-agent `agent_id` /
`qos` fields, and `getIoDetails`).

### Ported ahead of the base

One item sits *outside* the range the base above describes, because it was
ported from in-development pydoover work rather than a released commit:

- **`getDOCurrent` / `fetch_do_current`** (2026-08-13) — per-output load
  current. Tracked by getdoover/pydoover#157; unreleased at time of porting.

So `platform_iface.proto` is **no longer byte-identical to 1.13.0**: it carries
one extra RPC and two extra messages. The proto's own header says so too.

This matters for the next sync: re-vendoring `platform_iface.proto` from any
pydoover release that predates #157 silently deletes `getDOCurrent`, and the
Rust that calls it stops compiling. Once #157 ships, re-vendor from a release
that contains it and delete this note. Until then, re-vendor by hand and keep
the three blocks.

## Deliberate divergences

Things pydoover does that doover-rs intentionally does **not** copy, so a future
sync doesn't "fix" them back:

- **`RPCManager.register_handlers` static attribute resolution** (pydoover
  `f22d470`) — pydoover has to avoid triggering `@property` getters while
  scanning an object for decorated handlers. Rust registers handlers explicitly,
  so there is nothing to scan and no descriptor to trip.
- **Runtime type validation** on `Notification` fields (pydoover raises
  `TypeError` for a non-`str` message/title/topic) — the compiler covers these.
  Only the checks types can't make survive, in `Notification::validate`.
- **`NotificationSeverity` on the wire stays an integer** in the notification
  payload, matching pydoover: the server has a hand-written deserialiser for
  severity that takes either form. `NotificationType` has no such deserialiser,
  so it goes by name (`wire()`).
- **`suppress_response` is set by `ChannelBackend::update_channel_aggregate`
  itself**, not left to callers. The trait method returns `()`, so the echoed
  aggregate is always discarded; pydoover arrives at the same place by having
  `TagsManagerProcessor` pass `suppress_response=True` at its one call site.
- **`Schema.name` vs `Schema._schema_name`** (pydoover `9584701`) — pydoover had
  to move the schema title off `cls.name` because a config element *named*
  `name` would shadow it as a class attribute. `SchemaModel::title` was always a
  distinct field, so there is nothing to collide.
- **`ModbusInterface._parse_register_output` wrapping in `list()`** (pydoover
  `35081cd`) — a protobuf repeated field is not a Python `list`, so pydoover's
  documented return type was a lie. prost generates a real `Vec<i32>`, which
  `read_registers` already returns.
- **`platform_iface` attached to `Tags`/`UI` before `setup()`** (pydoover
  `51d436f`) — pydoover binds the handle late because its runner rebinds
  `app.config`, which can leave Tags/UI holding a stale instance, and because
  Tags and UI are separate objects with their own `setup()`. doover-rs has one
  `Application::setup(&mut self, ctx)`, the app owns its `PlatformClient`
  directly (see `examples/level_sensor.rs`), and `UiBuild::build` is a pure
  constructor — there is no second object to keep fresh and no rebinding to go
  stale. Ownership covers what the late attach is defending against.
- **A conditional config element needs `Option<T>` or a default.** pydoover
  leaves an inactive `show_if` element as `NotSet` and raises only if you read
  it; a Rust struct field always holds something. So a conditional field is
  declared `Option<T>` (reading `None` when inactive) and adds
  `#[config(required)]` if it must still be demanded while active — see
  `doover/tests/config_schema_fixtures.rs`. The emitted schema is identical
  either way.
- **`Condition` names its controller by `x-name`, never by element identity.**
  pydoover's `config.equal()` accepts the `ConfigElement` object and resolves it
  back to a name by scanning for identity; there is no such object graph to scan
  here. `#[config(show_if_eq(field, value))]` takes the sibling's *Rust field*
  and the macro resolves it to that field's config key, so a
  `#[config(name = "…")]` override still lines up.
- **`ui.Application`'s `full_width` kwarg** (pydoover `6394c84`,
  `ui/submodule.py`) — `ui.Application` is deliberately not ported at all (see
  the `ui::submodule` module docs): the root `uiApplication` node comes from
  `UiTree::to_schema`, which *does* emit `fullWidth`. Only the non-declarative
  element is missing, not the behaviour.
- **Stream keepalive is one endpoint setting, not per-call channel options.**
  The DDA channel sets `keep_alive_while_idle(true)` and carries both unary
  calls and event streams; pydoover needs a separate `_STREAM_CHANNEL_OPTIONS`
  channel per stream. The sidecar clients do get a dedicated
  `SharedChannel::stream_channel` with pydoover's 60 s cadence — see the
  comment there for its coupling to doover-platform-interface's 30 s ping floor.

## Not ported yet

See the "Still to port" list in [`README.md`](README.md#roadmap-mirroring-the-pydoover-surface).
Short version: cloud auth beyond bearer tokens, declarative processor-config
authoring (`dv_proc_config` schema elements — which is where pydoover's
`DataPermissions` / `dd_permissions` element would land), and the cloud
management surface (batch endpoints, the Control API, ingestion-endpoint
management).

Changes in the 1.11.4 → 1.13.0 range that landed inside those gaps, so they are
waiting on the gap rather than on themselves:

- **Batch message/aggregate mutations** (pydoover `aac87e1`):
  `BatchMutationItem` / `BatchMutationResult` / `BatchMutationResponse`,
  `MAX_BATCH_MUTATIONS = 50`, and `batch_{create,update,delete}_messages` /
  `batch_update_aggregates` on `PATCH|POST|PUT|DELETE /agents/messages` and
  `PATCH /agents/aggregates`. doover-rs' `DataClient` has never carried the batch
  surface — the batch *read* endpoints (`BatchMessageResponse`,
  `BatchAggregateResponse`) predate this sync's base commit and are also absent —
  so these are a new leaf on a missing tree.
- **`ControlClient.mint_registry_token`** (pydoover `9668ad3`) —
  `POST /applications/{id}/registry_token/`, returning short-lived
  `docker login` credentials scoped to one application's image. doover-rs has no
  Control API client at all.
- **`origin` on `put_ingestion_endpoint`** (pydoover `b44f365`) — names the
  external system (an AWS IoT rule destination ARN) allowed to complete an
  ingestion's unauthenticated setup handshake; omitting it clears any registered
  origin. doover-rs does not manage ingestion endpoints.
- **Report-generator failed state and log capture** (pydoover `647c91e`,
  `9ec240e`) — `_report_metadata["status"] = "Failed"` on a generation error, and
  a root-logger `StringIO` handler (tagged `_report_log_capture`, and removed if
  left over in a warm lambda container) feeding the report message's `logs`
  field. `pydoover/reports/` is not ported.
