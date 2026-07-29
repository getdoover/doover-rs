# Parity with pydoover

doover-rs is a port of [`pydoover`](https://github.com/getdoover/pydoover). This
file records **how far the port has caught up**, so the next sync knows exactly
where to start reading pydoover's history from instead of guessing at dates.

Keep it accurate — the `sync-pydoover-parity` skill reads `Synced to` below as
its diff base and rewrites this file when it finishes.

## Synced to

| | |
|---|---|
| pydoover commit | `d755172a7744dda9fd50c48efd9b995c10ce7e9d` |
| pydoover version | 1.11.4 |
| Date of sync | 2026-07-29 |

The vendored protos under `doover-proto/proto/` were verified byte-identical
(comments aside) to `pydoover/protos/` at that commit.

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
- **Stream keepalive is one endpoint setting, not per-call channel options.**
  The DDA channel sets `keep_alive_while_idle(true)` and carries both unary
  calls and event streams; pydoover needs a separate `_STREAM_CHANNEL_OPTIONS`
  channel per stream. The sidecar clients do get a dedicated
  `SharedChannel::stream_channel` with pydoover's 60 s cadence — see the
  comment there for its coupling to doover-platform-interface's 30 s ping floor.

## Not ported yet

See the "Still to port" list in [`README.md`](README.md#roadmap-mirroring-the-pydoover-surface).
Short version: cloud auth beyond bearer tokens, and declarative
processor-config authoring (`dv_proc_config` schema elements — which is where
pydoover's `DataPermissions` / `dd_permissions` element would land).
