#!/usr/bin/env python3
"""Generate config-schema fixtures from pydoover.

`doover/src/config/schema.rs` must reproduce pydoover's JSON-Schema emission
byte for byte, including key order and the `allOf` `if`/`then` branches
conditional fields (`show_if`) compile to. Rather than hand-writing the expected
JSON from reading `pydoover/config/__init__.py`, this script runs the reference
implementation over a corpus of schemas and dumps what it actually produced to
tests/compat/fixtures/config_schemas.json, which
doover/tests/config_schema_fixtures.rs replays.

Each case also records `load_data` outcomes, since a conditional element changes
what a deployment config is *required* to contain, not just what it emits.

Regenerating fixtures is a deliberate, reviewed act — run this only when
pydoover's behaviour intentionally changes, and review the fixture diff:

    PYTHONPATH=../pydoover python3 scripts/gen_config_schema_fixtures.py
"""

import json
import sys
from pathlib import Path

import pydoover.config as config

OUT_DIR = Path(__file__).parents[1] / "tests" / "compat" / "fixtures"


def case(name, factory, loads=()):
    """One schema, its emitted JSON Schema, and how it loads sample configs.

    `factory` must build a FRESH schema class per call. pydoover's config
    elements are class attributes, so one class reused across cases carries the
    previous case's loaded values forward — an artifact of Python's descriptor
    model, not behaviour to encode in a fixture.
    """
    results = []
    for data in loads:
        schema_cls = factory()
        instance = schema_cls()
        try:
            instance._inject_deployment_config(dict(data))
        except Exception as e:  # noqa: BLE001 - the error text is the fixture
            results.append({"config": data, "error": f"{type(e).__name__}: {e}"})
            continue
        loaded = {}
        for key, element in schema_cls._element_map.items():
            # An inactive conditional element with no default stays NotSet, and
            # pydoover's `.value` raises rather than inventing one — record that
            # as "unset" instead of a value.
            try:
                loaded[key] = element.value
            except ValueError:
                loaded[key] = "<unset>"
        results.append({"config": data, "loaded": loaded})
    return {"name": name, "schema": factory().to_schema(), "loads": results}


CASES = []

# -- A single conditional field on an enum controller ------------------------
# The controller's default satisfies the condition, so the `if` has no
# `required`: JSON Schema's `properties` already matches an absent property, and
# an omitted controller does default to "radar".


def default_matches():
    class DefaultMatches(config.Schema):
        mode = config.Enum("Mode", choices=["radar", "submersible"], default="radar")
        depth = config.Number("Depth", show_if=config.equal("mode", "radar"))

    return DefaultMatches


CASES.append(
    case(
        "default_matches_condition",
        default_matches,
        loads=[{}, {"mode": "radar", "depth": 1.5}, {"mode": "submersible"}],
    )
)


# -- The controller's default does NOT satisfy the condition ----------------
# Here the `if` gains `required: [mode]`, or the branch would fire on a config
# that omits the controller entirely.


def default_differs():
    class DefaultDiffers(config.Schema):
        mode = config.Enum(
            "Mode", choices=["radar", "submersible"], default="submersible"
        )
        depth = config.Number("Depth", show_if=config.equal("mode", "radar"))

    return DefaultDiffers


CASES.append(
    case(
        "default_differs_from_condition",
        default_differs,
        loads=[{}, {"mode": "radar", "depth": 2.5}, {"mode": "radar"}],
    )
)


# -- Several fields sharing one condition share one branch, in order --------


def shared_branch():
    class SharedBranch(config.Schema):
        mode = config.Enum("Mode", choices=["radar", "submersible"], default="radar")
        depth = config.Number("Depth", show_if=config.equal("mode", "radar"))
        offset = config.Number(
            "Offset", default=0.0, show_if=config.equal("mode", "radar")
        )
        probe = config.String("Probe", show_if=config.equal("mode", "submersible"))
        always = config.Boolean("Always", default=True)

    return SharedBranch


CASES.append(
    case(
        "shared_and_distinct_branches",
        shared_branch,
        loads=[
            {"mode": "radar", "depth": 1.0},
            {"mode": "submersible", "probe": "p1"},
        ],
    )
)


# -- A boolean controller, and a conditional field nested in an Object ------


def nested_object():
    tuning = config.Object("Tuning", default={})
    tuning.add_elements(
        config.Boolean("Use Filter", default=False),
        config.Number(
            "Filter Alpha", default=0.5, show_if=config.equal("use_filter", True)
        ),
        config.Integer("Filter Window", show_if=config.equal("use_filter", True)),
    )

    class NestedObject(config.Schema):
        advanced_mode = config.Boolean("Advanced Mode", default=False)
        threshold = config.Number(
            "Threshold", show_if=config.equal("advanced_mode", True)
        )

    NestedObject.tuning = tuning
    NestedObject._load_elements()
    return NestedObject


CASES.append(
    case(
        "nested_object_conditions",
        nested_object,
        loads=[
            {},
            {"advanced_mode": True, "threshold": 9.0,
             "tuning": {"use_filter": True, "filter_window": 3}},
        ],
    )
)


# -- The full-width application element (pydoover 6394c84) -----------------


def app_elements():
    class AppElements(config.Schema):
        interpreter_hidden = config.ApplicationInterpreterHidden()
        interpreter_full_width = config.ApplicationFullWidth()
        dv_app_position = config.ApplicationPosition()
        dv_app_default_open = config.ApplicationDefaultOpen()

    return AppElements


CASES.append(case("application_elements", app_elements, loads=[{}]))


def main():
    OUT_DIR.mkdir(parents=True, exist_ok=True)
    out = OUT_DIR / "config_schemas.json"
    out.write_text(json.dumps(CASES, indent=2) + "\n")
    print(f"wrote {len(CASES)} cases to {out}", file=sys.stderr)


if __name__ == "__main__":
    main()
