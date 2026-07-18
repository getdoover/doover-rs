# doover-cli

`doover` — the Rust replacement for the pydoover CLI. A thin shell over the
three device-sidecar gRPC interfaces:

```
doover device_agent <cmd> [args...]   # Device Agent (DDA), port 50051
doover platform     <cmd> [args...]   # Platform interface, port 50053
doover modbus       <cmd> [args...]   # Modbus interface, port 50054
```

## Options

URIs and the app key resolve the same way the app runtime does — flag beats
env var beats default:

| Flag           | Env var      | Default           |
| -------------- | ------------ | ----------------- |
| `--dda-uri`    | `DDA_URI`    | `localhost:50051` |
| `--plt-uri`    | `PLT_URI`    | `localhost:50053` |
| `--modbus-uri` | `MODBUS_URI` | `localhost:50054` |
| `--app-key`    | `APP_KEY`    | `pydoover-cli`    |

These are global (valid anywhere on the command line). Each section also
accepts pydoover's `--uri` immediately after the section name
(`doover platform --uri 10.0.0.5:50053 fetch_ai 0`), which overrides the
matching global flag. `--debug` enables verbose tracing on stderr.

## Output

Results are printed as JSON on stdout (one value per line); commands with no
result (e.g. `reboot`) print nothing. Errors are
human-readable on stderr with a nonzero exit code. `listen_channel` streams
one JSON object per event line and reconnects on stream failure.

JSON payload arguments are inline JSON strings:

```sh
doover device_agent update_channel_aggregate tag_values '{"level": 42}' --save_log
doover device_agent create_message my_channel '{"hello": "world"}'
doover device_agent list_messages my_channel --limit 5
doover device_agent listen tag_values
doover platform fetch_ai 0 1 2
doover platform set_do '[0,1]' 1        # single value broadcasts to all pins
doover modbus read_registers --modbus_id 1 --start_address 0 --num_registers 2
```

## Parity with the pydoover CLI

Subcommand and argument names match pydoover's `@cli_command` surface
(snake_case, e.g. `fetch_channel_aggregate`, `--replace_data`); kebab-case
aliases are also accepted (`fetch-channel-aggregate`, `--replace-data`), plus
short aliases `get_aggregate`, `update_aggregate`, `send_oneshot` and
`listen`.

Covered:

- **device_agent**: `test_comms`*, `get_is_dda_available`, `get_is_dda_online`,
  `get_has_dda_been_online`, `fetch_channel_aggregate`,
  `update_channel_aggregate`, `create_message`, `send_oneshot_message`,
  `fetch_message`, `list_messages`, `update_message`, `fetch_turn_token`,
  `listen_channel`.
- **platform**: `test_comms`, `fetch_di`/`fetch_ai`/`fetch_do`/`fetch_ao`
  (one or more pins), `set_do`/`set_ao`, `schedule_do`/`schedule_ao`,
  `fetch_system_voltage`, `fetch_system_power`, `fetch_system_temperature`,
  `fetch_location`, `reboot`, `shutdown`, `fetch_immunity_seconds`,
  `set_immunity_seconds`, `fetch_wake_on_voltage`, `set_wake_on_voltage`,
  `fetch_wake_reason`, `fetch_sleep_log`, `fetch_sleep_log_interval`,
  `set_sleep_log_interval`, `schedule_shutdown`, `schedule_startup`*,
  `fetch_io_table`, `sync_rtc`, `fetch_di_events`, `fetch_di_config`,
  `set_di_config`.
- **modbus**: `test_comms`, `open_bus`, `close_bus`, `fetch_bus_status`,
  `list_buses`*, `read_registers`, `write_registers`.

(* = not in pydoover's CLI surface; exposed here because the Rust client has it.)

Every argument pydoover's argparse accepted still parses, so command lines can
move across unedited. Some no longer select anything and are accepted, hidden
from `--help`, and ignored:

- `--json` (results are always JSON) and `--enable-traceback` (see `--debug`).
- `--service_name`, `--dda_timeout`, `--max_conn_attempts`,
  `--time_between_connection_attempts`, `--timeout`, `--config` — the Rust
  clients dial on demand and take the service name from the compiled-in proto.
- `read_registers`/`write_registers`' `--bus_id`, `--configure_bus` and `--bus`
  — the sidecar identifies a bus by its connection settings and opens it on
  demand, so `bus_id` was already deprecated-and-ignored in pydoover.
- `--files` on the message/aggregate writes — pydoover's argparse type for it
  mangled whatever it was given, so it could never carry an attachment.

`--shell` is the one exception: it's rejected, not ignored. It existed only to
amortize Python's ~0.8s interpreter startup across calls, which Rust doesn't
pay, so one-off invocations are already faster than pydoover's shell-mode steady
state. Clients that probe for it (`doover-cockpit`'s `CockpitShellTransport`)
treat the rejection as "unsupported" and fall back to one-off spawns.

Known behavioural differences:

- Results are always JSON (pydoover prints Python reprs unless `--json`).
- The DDA status commands (`get_is_dda_online` etc.) issue a comms check
  first so the flags reflect the live agent (pydoover's CLI reports the
  freshly-constructed interface's flags, which are always `False`).
- JSON payload arguments must be valid JSON; pydoover silently fell back to
  passing the raw string through.
- Where pydoover accepted an ISO-8601 datetime (`list_messages
  --before/--after`, `send_oneshot_message --timestamp`) so does this, and unix
  milliseconds are accepted too. A naive datetime is read in the local
  timezone, matching `datetime.fromisoformat`.
