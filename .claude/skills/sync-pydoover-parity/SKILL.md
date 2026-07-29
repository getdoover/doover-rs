---
name: sync-pydoover-parity
description: Bring doover-rs (the Rust Doover client) up to date with everything that has changed in pydoover since the last sync — read pydoover's history from the recorded parity commit, audit doover-rs for each change, port the gaps, prove the port against pydoover-generated fixtures, and update PARITY.md + the README roadmap. TRIGGER on "sync doover-rs with pydoover", "port the pydoover changes to doover-rs", "bring doover-rs up to date", "check doover-rs parity", "what's pydoover got that doover-rs doesn't", "/sync-pydoover-parity". NOT for writing a new Doover app (use doover-device-apps / doover-appgen) or for changing pydoover itself.
user_invocable: true
---

# Sync doover-rs with pydoover

`doover-rs` is a hand-written Rust port of `pydoover`. pydoover is the source of
truth and moves faster. This skill closes the gap, and is designed to be run
repeatedly — every run leaves the next run a clean starting point.

**Both repos live under `~/Documents/refactor/work/getdoover/`** (`pydoover/`,
`doover-rs/`). Resolve them there; if either is missing, ask rather than guess.

## Ground rules

- **pydoover wins on behaviour.** doover-rs replicates pydoover's wire bytes,
  key order, defaults and quirks. When they disagree, pydoover is right unless
  `doover-rs/PARITY.md` lists the difference as deliberate.
- **Read `doover-rs/PARITY.md` first, and `PARITY.md`'s "Deliberate
  divergences" before "fixing" anything.** A surprising difference is often a
  recorded decision.
- **Never `cargo fmt` the repo.** doover-rs was written to a wider style than
  default rustfmt and has no `rustfmt.toml`, so `cargo fmt` rewrites every file
  and buries the real diff. Match surrounding style by hand. (`cargo clippy` is
  fine and should stay clean.)
- **Don't modify pydoover, and don't leave other repos dirty.** If you generate
  fixtures by running a Doover app's export (see step 5), `git checkout` that
  app afterwards.

## Step 1 — Establish the diff base

```sh
cd ~/Documents/refactor/work/getdoover/pydoover && git fetch --all -q
```

Read `Synced to` in `doover-rs/PARITY.md` for the base commit `$BASE`. Take
`origin/main` as `$HEAD` — that is the released client, and it is what apps get.
(Local pydoover branches may be ahead or behind; note it if HEAD has unmerged
work, but sync against `origin/main`.)

If `PARITY.md` is absent or its base looks wrong, fall back to dating: the
newest `doover-rs` commit tells you roughly when the port last tracked pydoover.
Say out loud which base you used.

## Step 2 — Enumerate what changed

```sh
git diff --stat $BASE origin/main -- pydoover protos
git log --format="%h %ad %s" --date=short $BASE..origin/main
```

Then read the actual diff, excluding generated protobuf stubs (they're derived
from the `.proto`, which you check separately):

```sh
git diff $BASE origin/main -- pydoover protos ':!*_pb2*'
```

Write the changes down as a checklist before touching Rust. Classify each as:

- **behavioural** — wire payload, defaults, key order, control flow. Must port.
- **new surface** — a new element/kwarg/enum variant/RPC. Port it.
- **Python-only** — descriptor/`inspect`/typing problems Rust doesn't have.
  Record in `PARITY.md` "Deliberate divergences" instead of porting.
- **inside a known gap** — a change to a pydoover subsystem doover-rs hasn't
  ported at all (see README "Still to port"). Note it in that gap's entry; don't
  port a leaf of a missing tree.

## Step 3 — Check the vendored protos

`doover-rs/doover-proto/proto/` is vendored verbatim. Verify each:

```sh
cd ~/Documents/refactor/work/getdoover
for f in device_agent health modbus_iface platform_iface; do
  echo "--- $f"
  diff <(grep -v '^//' doover-rs/doover-proto/proto/$f.proto) \
       <(grep -v '^//' pydoover/protos/$f.proto)
done
```

Any difference means re-vendor (copy pydoover's file, restore the `// Vendored
VERBATIM …` header, update its version/commit note). A proto change usually
implies matching Rust in `doover/src/docker/device_agent.rs`.

## Step 4 — Audit doover-rs per item, then port

Baseline first, so you know a later failure is yours:

```sh
cd ~/Documents/refactor/work/getdoover/doover-rs && cargo test --all-features
```

For each checklist item, grep for the identifier before assuming it's missing —
some things landed in doover-rs already, occasionally under a Rust-idiomatic
name. `references/mapping.md` maps pydoover modules to their Rust homes and
lists the naming translations that recur.

Port in the repo's established idiom:

- Python keyword arguments → an options struct with `Default` + builder methods
  (`CallOptions`, `AggregateOptions`, `SubscribeOptions`), not long positional
  argument lists. Keep existing public signatures working where you can.
- Python sentinels (`NotSet`, `NOT_GIVEN`) → `Option<T>`, where `Some(Null)` and
  `None` mean what the sentinel distinguished.
- `IntEnum`s the API represents by name → a Rust enum with `wire()`, `value()`,
  `from_value()`, and `FromStr` carrying pydoover's alias set.
- Every ported item gets a doc comment naming its pydoover counterpart and, when
  the behaviour is non-obvious, *why* it is that way. That is how the port stays
  auditable — carry pydoover's reasoning across, don't just its code.

## Step 5 — Prove it against pydoover, not against your reading

This is the part that makes the sync trustworthy. Don't hand-write expected JSON
from reading Python; generate it.

**UI elements / log triggers / diffs / snowflakes** — extend the generator under
`doover-rs/scripts/` with cases exercising the new fields, then run it against
the live pydoover checkout and inspect the diff:

```sh
cd ~/Documents/refactor/work/getdoover/doover-rs
PYTHONPATH=../pydoover python3 scripts/gen_ui_element_fixtures.py
git diff tests/compat/fixtures/
```

Add the matching arm to the Rust replay test (`doover/tests/ui_element_fixtures.rs`
and friends) and raise its `cases.len() >= N` floor.

**Config / UI schema goldens** — `doover/tests/fixtures/analog_level_sensor_*.json`
are byte-exact pydoover exports. If a change touches schema emission, regenerate
from the real app and diff:

```sh
cd ~/Documents/refactor/work/getdoover/apps/analog-level-sensor
python3 -c "
import sys; sys.path.insert(0,'../../pydoover'); sys.path.insert(0,'src')
from analog_level_sensor.app_ui import export; export()"
git diff doover_config.json      # <- this is the expected change
git checkout doover_config.json  # <- leave the app repo clean
```

Copy just that change into the doover-rs fixture. A golden diff you cannot
explain from the pydoover changelog means you've misread something — stop and
re-read.

Then:

```sh
cd ~/Documents/refactor/work/getdoover/doover-rs
cargo test --all-features && cargo clippy --all-features --all-targets
```

## Step 6 — Record the new state

1. `PARITY.md`: new `Synced to` commit + pydoover version + date; append any
   new deliberate divergences with their reasoning.
2. `README.md` roadmap: move ported items into the "Done (caught up to pydoover
   X.Y.Z)" list with a one-line summary; update "Still to port" entries that
   grew a new leaf.
3. `Cargo.toml`: bump `workspace.package.version` patch level.

## Step 7 — Report

Tell the user, concretely:

- the pydoover range synced (`$BASE..$HEAD`, version to version);
- each change ported, one line each, with the file it landed in;
- each change **not** ported and why (Python-only, or inside a known gap) —
  never let a skipped item go unmentioned;
- test/clippy status, and any fixture that had to be regenerated.

Do not commit unless asked. If asked, one commit per logical change reads far
better than one giant "sync pydoover" commit.
