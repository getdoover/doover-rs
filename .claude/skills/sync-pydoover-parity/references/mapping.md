# pydoover → doover-rs map

Where a pydoover change lands on the Rust side. Paths are relative to
`~/Documents/refactor/work/getdoover/`.

## Module map

| pydoover | doover-rs |
|---|---|
| `pydoover/rpc.py` | `doover/src/rpc.rs` |
| `pydoover/models/data/notification.py` | `doover/src/models.rs` |
| `pydoover/models/data/channel.py` (listings) | `doover/src/docker/device_agent.rs` (`ChannelListing`/`ChannelList`) |
| `pydoover/models/data/processor_info.py`, `connection.py` | `doover/src/models.rs` (feature `cloud-api`) |
| `pydoover/config/__init__.py` | `doover/src/config/mod.rs` (marker types), `config/schema.rs` (JSON-Schema emission), `config/export.rs` (`doover_config.json` merge-write) |
| `pydoover/ui/element.py`, `variable.py` | `doover/src/ui/element.rs`, `ui/variable.rs` |
| `pydoover/ui/interaction.py` | `doover/src/ui/interaction.rs` |
| `pydoover/ui/parameter.py` | `doover/src/ui/parameter.rs` |
| `pydoover/ui/misc.py` | `doover/src/ui/misc.rs` |
| `pydoover/ui/submodule.py` | `doover/src/ui/submodule.rs` |
| `pydoover/ui/camera.py` | `doover/src/ui/camera.rs` |
| `pydoover/ui/declarative.py` | `doover/src/ui/mod.rs` (`UiApplicationInfo`, `UiTree`) + the `Ui` derive in `doover-macros/` |
| `pydoover/ui/manager.py` | `doover/src/ui/runtime.rs` (`UiRuntime`); the `call` override is `AppContext::call_ui_command` |
| `pydoover/tags/manager.py` (`TagsManagerDocker`) | `doover/src/tags/runtime.rs` |
| `pydoover/tags/manager.py` (`TagsManagerProcessor`) | `doover/src/processor/tags.rs` |
| tag `log_on` triggers | `doover/src/tags/triggers.rs` |
| `pydoover/docker/application.py` | `doover/src/docker/application.rs` (`Application` trait, `AppContext`, `doover::run`) |
| `pydoover/docker/device_agent/device_agent.py` | `doover/src/docker/device_agent.rs` + `docker/subscriptions.rs` (the stream/reconnect half) |
| `pydoover/docker/grpc_interface.py` | `doover/src/docker/grpc.rs` (`SharedChannel`) |
| `pydoover/docker/platform/` | `doover/src/docker/platform.rs` |
| `pydoover/docker/modbus/` | `doover/src/docker/modbus.rs` |
| `pydoover/api/data/_async.py` | `doover/src/api/data.rs` (feature `cloud-api`) |
| `pydoover/api/auth/` | `doover/src/api/auth.rs` — bearer tokens only, see README "Still to port" |
| `pydoover/processor/` | `doover/src/processor/` (feature `processor`) |
| `pydoover/utils/diff.py`, `snowflake.py` | `doover/src/utils/diff.rs`, `utils/snowflake.rs` |
| device-agent CLI (`@cli_command`) | `doover-cli/src/` |
| `protos/*.proto` | `doover-proto/proto/*.proto` (vendored verbatim) |

Not ported at all, so changes to them are gap notes rather than work:
`pydoover/reports/`, `pydoover/state/`, `pydoover/utils/{kalman,pid,alarm}.py`,
`pydoover/cli/` (doover-cli is a separate design), and the declarative
`dv_proc_config` authoring elements in `pydoover/processor/config.py`.

## Naming translations that recur

| pydoover | doover-rs |
|---|---|
| keyword arguments | an options struct: `CallOptions`, `AggregateOptions`, `UpdateMessageOptions`, `SubscribeOptions`, `SetTagOptions`, `ListMessagesOptions` |
| `NotSet` / `NOT_GIVEN` sentinel | `Option<T>` (`Some(Value::Null)` = explicit null, `None` = key omitted) |
| `bool | X` union kwarg (`requires_confirm`, `audit`) | a two-variant enum + `From` impls (`Confirm`, `Audit`) |
| `ui.Option` | `SelectOption` (`Option` is taken) |
| `IntEnum` the API sends by name | enum + `wire()` / `value()` / `from_value()` / `FromStr` with pydoover's aliases |
| `Location`, `Aggregate`, `Message` dataclasses | plain structs with `to_json` / `from_proto`-style constructors |
| `raise ValueError/TypeError` on bad payload | `DooverError::InvalidPayload` from a `validate()` the send path calls |

## Fixture / golden inventory

Regenerating any of these is a deliberate, reviewed act — always inspect the
diff and be able to name the pydoover commit that caused it.

| Fixture | Generator | Rust replay |
|---|---|---|
| `tests/compat/fixtures/ui_elements.json` | `scripts/gen_ui_element_fixtures.py` | `doover/tests/ui_element_fixtures.rs` |
| `tests/compat/fixtures/log_triggers.json` | `scripts/gen_log_trigger_fixtures.py` | `doover/tests/log_trigger_fixtures.rs` |
| `tests/compat/fixtures/{diffs,snowflakes,payload_validation}.json` | `scripts/gen_compat_fixtures.py` | `doover/tests/compat_fixtures.rs` |
| `doover/tests/fixtures/analog_level_sensor_doover_config.json` | the real app's `export-ui` / `export-config` (see SKILL.md step 5) | `doover/tests/{config,ui,export}_golden.rs` |
| `doover/tests/fixtures/analog_level_sensor_app_config.json` | the app's `simulators/app_config.json` | `doover/tests/config_golden.rs` |

Generators run against whatever pydoover `PYTHONPATH` resolves, so point them at
the checkout, not an installed wheel:

```sh
PYTHONPATH=../pydoover python3 scripts/gen_ui_element_fixtures.py
```

## Behaviour worth re-reading before you touch it

The parts that are easy to break silently, and where the port has already been
made faithful:

- **`data_json` codec** — subscriptions request `WIRE_FORMAT_JSON_ONLY`, so the
  agent never builds the lossy protobuf `Struct`. `serde_json` is built with
  `preserve_order` so object key order matches Python dict insertion order.
- **Key order in every `to_json`** — the golden tests compare *serialized
  strings*. Inserting a new key in the wrong place fails them, correctly.
- **Position counter** — pydoover's global element counter starts at 50 and
  increments per element constructed, children before their container;
  `assign_positions_depth_first` replicates that.
- **`$config.app()` / `$tag.app()` / `$cmds.app()` reference strings**,
  including the `:type:default` suffixes and pydoover's historical doubled
  `:boolean:false` on `hidden`.
- **Tag write buffering** — one commit per loop, `only_if_changed` diffing,
  max-age 3 s while the app is open vs 900 s otherwise.
