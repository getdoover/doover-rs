#!/usr/bin/env python3
"""Generate UI-element serialization fixtures from pydoover.

Instantiates every UI element ported to doover-rs with representative
parameter combinations (NotSet vs None vs explicit values, positions,
conditions, tag references, nested containers), calls `.to_dict()` on the
reference implementation, and dumps the results to
tests/compat/fixtures/ui_elements.json. The Rust test
(doover/tests/ui_element_fixtures.rs) constructs the equivalent builders
per case id and asserts `to_json()` equality INCLUDING key order.

pydoover's `Element.__global_position_counter` is process-global; it is
reset to 50 before each case so every case's positions are deterministic
(a standalone element gets 51; a container's children get 51, 52, … and
the container itself the next slot — children are constructed first).

Regenerating fixtures is a deliberate, reviewed act:

    uv run --project ../pydoover python scripts/gen_ui_element_fixtures.py
"""

import json
from datetime import datetime, timedelta, timezone
from pathlib import Path

from pydoover import ui
from pydoover.ui.declarative import UITagBinding
from pydoover.ui.element import ConnectionInfo, ConnectionType, Element, Multiplot

OUT_PATH = Path(__file__).parents[1] / "tests" / "compat" / "fixtures" / "ui_elements.json"


def reset_position_counter():
    setattr(Element, "_Element__global_position_counter", 50)


def tag(name, tag_type=None, default=..., live=False):
    """A UITagBinding; `default=...` means no default (pydoover _MISSING)."""
    if default is ...:
        return UITagBinding(name, tag_type=tag_type, live=live)
    return UITagBinding(name, tag_type=tag_type, default_value=default, live=live)


def build_cases():
    """Return [(case_id, element_factory)]."""
    dt = datetime(2026, 1, 2, 3, 4, 5, tzinfo=timezone.utc)

    def submodule_children():
        return [
            ui.NumericVariable("Speed", value=tag("speed", "number", None, live=True)),
            ui.Button("Reset"),
        ]

    return [
        # ---- containers (pydoover ui/submodule.py) ----
        ("container_plain", lambda: ui.Container("Group", submodule_children())),
        (
            "container_explicit_position_child",
            lambda: ui.Container(
                "Group",
                [ui.TextVariable("Status", value=None, position=7), ui.Switch("Pump")],
                position=99,
            ),
        ),
        (
            "container_nested",
            lambda: ui.Container(
                "Outer",
                [
                    ui.NumericVariable("Top"),
                    ui.Container("Inner", [ui.Button("Deep")]),
                    ui.BooleanVariable("Flag", value=None),
                ],
            ),
        ),
        ("submodule_plain", lambda: ui.Submodule("Pump Details", submodule_children())),
        (
            "submodule_status_and_collapsed",
            lambda: ui.Submodule("Pump Details", [], status="OK", is_collapsed=True),
        ),
        (
            "submodule_status_none",
            lambda: ui.Submodule("Pump Details", [], status=None),
        ),
        (
            "submodule_default_open_beats_is_collapsed",
            lambda: ui.Submodule(
                "Pump Details", [], is_collapsed=True, default_open=True
            ),
        ),
        (
            "submodule_default_open_tag",
            lambda: ui.Submodule(
                "Pump Details", [], default_open=tag("pump_open", "boolean", False)
            ),
        ),
        ("tabs_plain", lambda: ui.TabContainer("Views", submodule_children())),
        (
            "tabs_default_page",
            lambda: ui.TabContainer("Views", [ui.TextVariable("A", value=None)], default_page=1),
        ),
        (
            "remote_component_plain",
            lambda: ui.RemoteComponent("Widget", "https://example.com/c.js"),
        ),
        (
            "remote_component_extras",
            lambda: ui.RemoteComponent(
                "Widget",
                "https://example.com/c.js",
                [ui.Button("Go")],
                foo="bar",
                answer=42,
            ),
        ),
        # ---- cameras (pydoover ui/camera.py) ----
        ("camera_live_view", lambda: ui.CameraLiveView("front", "hls", True)),
        (
            "camera_live_view_named",
            lambda: ui.CameraLiveView(
                "front", "rtsp", False, display_name="Front Camera"
            ),
        ),
        ("camera_history", lambda: ui.CameraHistory("front")),
        (
            "camera_history_named",
            lambda: ui.CameraHistory("back", display_name="Back History"),
        ),
        # ---- connection info (pydoover ui/element.py) ----
        ("connection_info_default", lambda: ConnectionInfo()),
        (
            "connection_info_periodic_full",
            lambda: ConnectionInfo(
                connection_type=ConnectionType.periodic,
                connection_period=600,
                next_connection=300,
                offline_after=3600,
                allowed_misses=3,
            ),
        ),
        (
            "connection_info_offline_after_only",
            lambda: ConnectionInfo(offline_after=7200),
        ),
        # ---- multiplot + series ----
        (
            "multiplot_minimal",
            lambda: Multiplot(
                "Trends", [ui.Series("Level", tag("level", "number", None))]
            ),
        ),
        (
            "multiplot_full",
            lambda: Multiplot(
                "Trends",
                [
                    ui.Series(
                        "Level",
                        tag("level", "number", None, live=True),
                        data_type="number",
                        active=True,
                        colour=ui.Colour.blue,
                        icon="droplet",
                        shared_axis=True,
                        units="%",
                        range=(0, "auto"),
                        ranges=[ui.Range("Low", 0, 15.0, ui.Colour.blue)],
                        thresholds=[ui.Threshold("High", 80, ui.Colour.red)],
                    ),
                    ui.Series(
                        "Pump State",
                        tag("pump", "boolean"),
                        name="pump_series",
                        data_type="boolean",
                        shared_axis="left",
                        step_labels=["Off", "On"],
                    ),
                    ui.Series("No Lookup", None),
                ],
                earliest_data_time=dt,
                default_zoom="7d",
                default_range_view=ui.RangeView.zone,
            ),
        ),
        # ---- timestamp (pydoover ui/variable.py) ----
        ("timestamp_unset", lambda: ui.Timestamp("Last Seen")),
        ("timestamp_int_ms", lambda: ui.Timestamp("Last Seen", value=1700000000000)),
        ("timestamp_datetime", lambda: ui.Timestamp("Last Seen", value=dt)),
        (
            "timestamp_precision_quirk",
            lambda: ui.Timestamp(
                "Next Run",
                value=1700000000000,
                precision="second",
                absolute_format="%Y-%m-%d %H:%M",
            ),
        ),
        (
            "timestamp_tag_live",
            lambda: ui.Timestamp(
                "Last Report", value=tag("last_report", "number", None, live=True)
            ),
        ),
        # ---- parameter inputs (pydoover ui/parameter.py) ----
        ("float_input_plain", lambda: ui.FloatInput("Target Level")),
        (
            "float_input_bounds_flavours",
            lambda: ui.FloatInput("Target Level", min_val=0, max_val=1.5),
        ),
        (
            "float_input_full_interaction",
            lambda: ui.FloatInput(
                "Target Level",
                min_val=-10.5,
                max_val=100,
                default=42,
                requires_confirm=True,
                show_activity=False,
                units="%",
            ),
        ),
        ("text_input_plain", lambda: ui.TextInput("Notes")),
        ("text_input_area", lambda: ui.TextInput("Notes", is_text_area=True)),
        ("datetime_input_plain", lambda: ui.DatetimeInput("Start At")),
        (
            "datetime_input_full",
            lambda: ui.DatetimeInput(
                "Start At",
                pickers=["date"],
                direction="past",
                max_past=timedelta(days=7),
                max_future=3600,
            ),
        ),
        ("time_input_plain", lambda: ui.TimeInput("Run At")),
        # ---- regression coverage for previously ported elements ----
        (
            "numeric_variable_full",
            lambda: ui.NumericVariable(
                "Level",
                value=tag("level", "number", None, live=True),
                precision=1,
                form=ui.Widget.radial,
                ranges=[ui.Range("Low", 0, 15.0, ui.Colour.blue)],
                units="%",
            ),
        ),
        ("button_with_default", lambda: ui.Button("Pump", default=True, disabled=False)),
        ("slider_defaults", lambda: ui.Slider("Speed Limit")),
        (
            "select_options",
            lambda: ui.Select(
                "Mode", options=[ui.Option("Fast Mode"), ui.Option("Slow Mode")]
            ),
        ),
        ("warning_indicator", lambda: ui.WarningIndicator("Low Level")),
    ]


def main():
    cases = []
    for case_id, factory in build_cases():
        reset_position_counter()
        element = factory()
        cases.append(
            {
                "case": case_id,
                "element": type(element).__name__,
                "expected": element.to_dict(),
            }
        )
    OUT_PATH.parent.mkdir(parents=True, exist_ok=True)
    OUT_PATH.write_text(json.dumps(cases, indent=2, ensure_ascii=False) + "\n")
    print(f"wrote {len(cases)} cases -> {OUT_PATH}")


if __name__ == "__main__":
    main()
