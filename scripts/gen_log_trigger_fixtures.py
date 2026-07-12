#!/usr/bin/env python3
"""Generate log-trigger evaluation fixtures from pydoover.

Feeds value sequences through pydoover's trigger evaluation — the real
path: a `Tags` subclass with `log_on=` declarations writing through a
recording manager, so declared defaults, per-tag state and the
`Tags._set_tag_value` promotion to `log=True` are all exercised — and
records the fire/no-fire decision per set. The Rust test
(doover/tests/log_trigger_fixtures.rs) replays the same sequences through
`doover::tags::TriggerSet` and asserts identical decisions.

Each case's `triggers` array is a machine-readable spec (kind + params) so
the Rust side constructs the triggers from the fixture itself.

Sequences are lifted from pydoover/tests/test_tags.py (TestNumericTriggers,
TestDirectionalNumericTriggers, TestDeltaTrigger, TestBooleanTriggers,
TestStringTriggers, TestCompositeTriggers) plus default-value and
non-numeric edge cases.

Regenerate deliberately:

    uv run --project ../pydoover python scripts/gen_log_trigger_fixtures.py
"""

import asyncio
import json
from pathlib import Path

from pydoover.tags import (
    AnyChange,
    Boolean,
    Cross,
    Delta,
    Enter,
    Exit,
    Fall,
    Number,
    Rise,
    String,
    Tags,
)

OUT_PATH = Path(__file__).parents[1] / "tests" / "compat" / "fixtures" / "log_triggers.json"

NOTSET = object()


class RecordingManager:
    """The minimal manager surface `Tags._set_tag_value` needs: reads with a
    default fallback and writes recording the effective `log` flag."""

    def __init__(self):
        self.values = {}
        self.log_flags = []

    def get_tag(self, key, default=None, app_key=None, raise_key_error=False):
        del raise_key_error
        return self.values.get((app_key, key), default)

    async def set_tag(self, key, value, app_key=None, **kwargs):
        self.log_flags.append(bool(kwargs.get("log")))
        self.values[(app_key, key)] = value


def spec_to_descriptor(spec):
    kind = spec["kind"]
    if kind in ("cross", "rise", "fall"):
        cls = {"cross": Cross, "rise": Rise, "fall": Fall}[kind]
        return cls(*spec["thresholds"], deadband=spec.get("deadband", 0.0))
    if kind == "delta":
        if "amount" in spec:
            return Delta(amount=spec["amount"])
        return Delta(percent=spec["percent"])
    if kind == "any_change":
        return AnyChange()
    if kind == "enter":
        return Enter(spec["value"])
    if kind == "exit":
        return Exit(spec["value"])
    raise ValueError(kind)


def tag_class(tag_type):
    return {"number": Number, "boolean": Boolean, "string": String}[tag_type]


async def run_case(case):
    descriptors = [spec_to_descriptor(s) for s in case["triggers"]]
    kwargs = {"log_on": descriptors}
    if "default" in case:
        kwargs["default"] = case["default"]
    template = tag_class(case["tag_type"])(**kwargs)
    cls = type("FixtureTags", (Tags,), {"t": template})

    manager = RecordingManager()
    tags = cls("app", manager, None)
    for value, explicit_log in zip(
        case["values"], case.get("explicit_log") or [False] * len(case["values"])
    ):
        await tags.t.set(value, log=explicit_log)
    return manager.log_flags


CASES = [
    # ---- Cross (pydoover TestNumericTriggers) ----
    {
        "case": "cross_initial_above_logs",
        "tag_type": "number",
        "triggers": [{"kind": "cross", "thresholds": [100]}],
        "values": [120],
    },
    {
        "case": "cross_initial_below_silent",
        "tag_type": "number",
        "triggers": [{"kind": "cross", "thresholds": [100]}],
        "values": [80],
    },
    {
        "case": "cross_up_then_down",
        "tag_type": "number",
        "triggers": [{"kind": "cross", "thresholds": [100]}],
        "values": [80, 120, 70],
    },
    {
        "case": "cross_no_relog_while_above",
        "tag_type": "number",
        "triggers": [{"kind": "cross", "thresholds": [100]}],
        "values": [120, 130, 110],
    },
    {
        "case": "cross_deadband_suppresses_oscillation",
        "tag_type": "number",
        "triggers": [{"kind": "cross", "thresholds": [50, 100], "deadband": 4}],
        "values": [40, 51, 49, 53, 49, 47],
    },
    {
        "case": "cross_multiple_thresholds_independent",
        "tag_type": "number",
        "triggers": [{"kind": "cross", "thresholds": [50, 100], "deadband": 4}],
        "values": [60, 110, 95, 40],
    },
    {
        "case": "cross_explicit_log_combines",
        "tag_type": "number",
        "triggers": [{"kind": "cross", "thresholds": [100]}],
        "values": [50],
        "explicit_log": [True],
    },
    {
        "case": "cross_float_int_mix",
        "tag_type": "number",
        "triggers": [{"kind": "cross", "thresholds": [15.0], "deadband": 1.0}],
        "values": [14.4, 15.6, 15.2, 14.4, 15],
    },
    {
        "case": "cross_ignores_non_numeric",
        "tag_type": "number",
        "triggers": [{"kind": "cross", "thresholds": [10]}],
        "values": [None, 15, None, 5],
    },
    # ---- Rise / Fall (TestDirectionalNumericTriggers) ----
    {
        "case": "rise_only_fires_up",
        "tag_type": "number",
        "triggers": [{"kind": "rise", "thresholds": [100]}],
        "values": [80, 120, 80, 120],
    },
    {
        "case": "fall_only_fires_down",
        "tag_type": "number",
        "triggers": [{"kind": "fall", "thresholds": [10]}],
        "values": [50, 5, 50, 5],
    },
    {
        "case": "rise_initial_above_fires",
        "tag_type": "number",
        "triggers": [{"kind": "rise", "thresholds": [100]}],
        "values": [120],
    },
    {
        "case": "fall_initial_below_silent",
        "tag_type": "number",
        "triggers": [{"kind": "fall", "thresholds": [10]}],
        "values": [5],
    },
    {
        "case": "rise_and_fall_composed",
        "tag_type": "number",
        "triggers": [
            {"kind": "rise", "thresholds": [100]},
            {"kind": "fall", "thresholds": [10]},
        ],
        "values": [50, 150, 5],
    },
    # ---- Delta (TestDeltaTrigger) ----
    {
        "case": "delta_first_set_seeds_baseline",
        "tag_type": "number",
        "triggers": [{"kind": "delta", "amount": 5}],
        "values": [80],
    },
    {
        "case": "delta_absolute_swing",
        "tag_type": "number",
        "triggers": [{"kind": "delta", "amount": 5}],
        "values": [80, 82, 83, 86, 89, 80],
    },
    {
        "case": "delta_percent_swing",
        "tag_type": "number",
        "triggers": [{"kind": "delta", "percent": 10}],
        "values": [100, 105, 110, 115, 99],
    },
    {
        "case": "delta_percent_zero_baseline",
        "tag_type": "number",
        "triggers": [{"kind": "delta", "percent": 10}],
        "values": [0, 0, 5, 5],
    },
    {
        "case": "delta_baseline_advances_only_on_fire",
        "tag_type": "number",
        "triggers": [{"kind": "delta", "amount": 5}],
        "values": [80, 82, 83, 85],
    },
    # ---- AnyChange / Enter / Exit (TestBooleanTriggers, TestStringTriggers) ----
    {
        "case": "any_change_each_transition",
        "tag_type": "boolean",
        "triggers": [{"kind": "any_change"}],
        "values": [True, False, True],
    },
    {
        "case": "any_change_repeat_is_silent",
        "tag_type": "boolean",
        "triggers": [{"kind": "any_change"}],
        "values": [True, True, False, False],
    },
    {
        "case": "any_change_default_matches_first_set",
        "tag_type": "boolean",
        "default": False,
        "triggers": [{"kind": "any_change"}],
        "values": [False, True],
    },
    {
        "case": "enter_exit_bidirectional_bool",
        "tag_type": "boolean",
        "triggers": [{"kind": "enter", "value": True}, {"kind": "exit", "value": True}],
        "values": [True, False, False],
    },
    {
        "case": "enter_only_fires_on_entry",
        "tag_type": "string",
        "triggers": [{"kind": "enter", "value": "error"}],
        "values": ["error", "warn"],
    },
    {
        "case": "exit_only_fires_on_exit",
        "tag_type": "string",
        "triggers": [{"kind": "exit", "value": "ok"}],
        "values": ["ok", "warn"],
    },
    {
        "case": "enter_exit_multi_value_composition",
        "tag_type": "string",
        "triggers": [
            {"kind": "enter", "value": "error"},
            {"kind": "exit", "value": "error"},
            {"kind": "enter", "value": "ok"},
            {"kind": "exit", "value": "ok"},
        ],
        "values": ["error", "warn", "ok", "warn"],
    },
    {
        "case": "composite_asymmetric_enter_exit",
        "tag_type": "string",
        "triggers": [{"kind": "enter", "value": "error"}, {"kind": "exit", "value": "ok"}],
        "values": ["ok", "warn", "error", "warn"],
    },
    {
        "case": "exit_with_default_prev",
        "tag_type": "string",
        "default": "ok",
        "triggers": [{"kind": "exit", "value": "ok"}],
        "values": ["warn", "ok", "warn"],
    },
    {
        "case": "string_repeat_set_is_silent",
        "tag_type": "string",
        "triggers": [{"kind": "enter", "value": "error"}],
        "values": ["error", "error"],
    },
]


def main():
    out = []
    for case in CASES:
        fired = asyncio.run(run_case(case))
        assert len(fired) == len(case["values"]), case["case"]
        record = dict(case)
        record["fired"] = fired
        out.append(record)
    OUT_PATH.parent.mkdir(parents=True, exist_ok=True)
    OUT_PATH.write_text(json.dumps(out, indent=2, ensure_ascii=False) + "\n")
    print(f"wrote {len(out)} cases -> {OUT_PATH}")


if __name__ == "__main__":
    main()
