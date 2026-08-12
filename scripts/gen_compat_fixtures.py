#!/usr/bin/env python3
"""Generate cross-language compatibility fixtures from pydoover.

doover-rs must match pydoover's behavior exactly for the shared wire
contracts (diff semantics, payload validation, tag-path handling, snowflake
layout). Rather than an IDL, the contract is machine-checked: this script
runs PYDOOVER (the reference implementation) over a corpus of inputs and
dumps its actual outputs to tests/compat/fixtures/, which the Rust test
suite (doover/tests/compat_fixtures.rs) replays and asserts against.

Note: `diffs.json` is not byte-stable across runs. pydoover's `generate_diff`
derives its deletion keys from a `set`, whose iteration order varies with
Python's string hash seed, so re-running this reshuffles the keys inside
delete-only diffs. That is cosmetic — the Rust replay compares JSON objects, not
key order — so revert such a diff rather than committing the churn.

Regenerating fixtures is a deliberate, reviewed act — run this only when
pydoover's behavior intentionally changes, and review the fixture diff:

    uv run --project ../pydoover python scripts/gen_compat_fixtures.py
"""

import itertools
import json
import sys
from pathlib import Path

from dataclasses import asdict

from pydoover.docker.platform import IoDetails
from pydoover.utils.diff import apply_diff, generate_diff
from pydoover.docker.device_agent.device_agent import validate_payload
from pydoover.utils.snowflake import (
    DOOVER_EPOCH,
    SnowflakeType,
    generate_snowflake_id_at,
)
from datetime import datetime, timezone

OUT_DIR = Path(__file__).parents[1] / "tests" / "compat" / "fixtures"

# A corpus of JSON documents chosen to hit the tricky paths: nested objects,
# nulls, scalars-vs-objects, empty objects, int/float/bool equality, arrays.
DOCS = [
    {},
    {"a": 1},
    {"a": 1, "b": 2, "c": 3},
    {"a": 1.0},
    {"a": True, "b": False},
    {"a": None},
    {"a": {"b": {"c": 1}}},
    {"a": {"b": 1}, "c": 2},
    {"a": {}},
    {"a": [1, 2, 3]},
    {"a": [1, {"x": 2}]},
    {"a": "str", "b": {"c": [True, None, 1.5]}},
    {"app": {"level": 5.5, "raw": 5, "on": True}},
    {"app": {"level": 6.0, "raw": 5, "on": False}, "other": {"x": None}},
]

# Diff payloads applied onto each doc (distinct from DOCS to exercise
# apply_diff's delete/replace paths).
DIFFS = [
    {},
    {"a": None},
    {"a": {"b": None}},
    {"a": {"x": None, "y": 1}},
    {"c": 4},
    {"a": {"b": {"c": 2, "d": None}}},
    "scalar-replacement",
    {"a": 5},
]


def gen_diff_fixtures():
    cases = []
    for old, new in itertools.product(DOCS, repeat=2):
        for do_delete in (True, False):
            diff = generate_diff(old, new, do_delete=do_delete)
            applied = apply_diff(old, diff, do_delete=do_delete)
            cases.append(
                {
                    "old": old,
                    "new": new,
                    "do_delete": do_delete,
                    "diff": diff,
                    "applied": applied,
                }
            )
    for data, diff in itertools.product(DOCS, DIFFS):
        for do_delete in (True, False):
            cases.append(
                {
                    "op": "apply",
                    "data": data,
                    "diff": diff,
                    "do_delete": do_delete,
                    "result": apply_diff(data, diff, do_delete=do_delete),
                }
            )
    return cases


PAYLOADS = [
    {"ok": 1},
    {"a-b_1": {"nested": [1, "s", True, None]}},
    {"bad key": 1},
    {"dots.bad": 1},
    {"": 1},
    {"a": {"bad key": 1}},
    {"a": [{"bad key": 1}]},
    [1, 2, 3],
    "string-root",
    5,
    None,
    {"unicode_ok": "价值"},
    {"ключ": 1},
]


def gen_payload_fixtures():
    cases = []
    for payload in PAYLOADS:
        try:
            validate_payload(payload)
            valid = True
        except ValueError:
            valid = False
        cases.append({"payload": payload, "valid": valid})
    return cases


def gen_snowflake_fixtures():
    cases = []
    for millis_offset, type_id, region, instance in [
        (0, SnowflakeType.Unknown, 0, 0),
        (1, SnowflakeType.Message, 0, 0),
        (123_456_789, SnowflakeType.Message, 3, 7),
        (86_400_000, SnowflakeType.Channel, 15, 1023),
        (999, SnowflakeType.OneShotMessage, 1, 1),
    ]:
        at = datetime.fromtimestamp(
            (DOOVER_EPOCH + millis_offset) / 1000, tz=timezone.utc
        )
        sid = generate_snowflake_id_at(
            at, type_id=type_id, region_id=region, instance_id=instance, use_rand=False
        )
        cases.append(
            {
                "unix_millis": DOOVER_EPOCH + millis_offset,
                "type_id": type_id,
                "region_id": region,
                "instance_id": instance,
                "snowflake": sid,
            }
        )
    return cases


# IO tables fed to `IoDetails.from_io_table`, the fallback a platform interface
# that predates getIoDetails takes. Covers the empty table, the usual
# int-per-type shape, numeric strings (pydoover's `int(ch)` accepts them), and
# out-of-order channels — `IoDetails.channels()` sorts by flat channel number.
IO_TABLES = [
    {},
    {"DI": [0, 1, 2], "DO": [0, 1], "AI": [0], "AO": []},
    {"DI": ["0", "1"], "AI": ["4"]},
    {"AI": [3, 1, 2], "DI": [9, 0]},
    {"DI": [0]},
]


def gen_io_details_fixtures():
    """`IoDetails.from_io_table` plus the accessors that read the result."""
    cases = []
    for table in IO_TABLES:
        details = IoDetails.from_io_table(table)
        cases.append(
            {
                "io_table": table,
                "details": asdict(details),
                "master": asdict(details.master) if details.master else None,
                # Flat channel numbers per type, in the order `channels()`
                # returns them (sorted by channel).
                "channels_by_type": {
                    io_type: [c.channel for c in details.channels(io_type)]
                    for io_type in ("DI", "DO", "AI", "AO")
                },
            }
        )
    return cases


def main():
    OUT_DIR.mkdir(parents=True, exist_ok=True)
    fixtures = {
        "diffs.json": gen_diff_fixtures(),
        "payload_validation.json": gen_payload_fixtures(),
        "snowflakes.json": gen_snowflake_fixtures(),
        "io_details.json": gen_io_details_fixtures(),
    }
    for name, cases in fixtures.items():
        path = OUT_DIR / name
        path.write_text(json.dumps(cases, indent=2, ensure_ascii=False) + "\n")
        print(f"wrote {len(cases):4d} cases -> {path.relative_to(Path.cwd())}")
    print("done", file=sys.stderr)


if __name__ == "__main__":
    main()
