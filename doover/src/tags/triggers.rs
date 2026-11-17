//! Log triggers — the port of pydoover's `log_on=` descriptors
//! (`Cross`/`Rise`/`Fall`/`Delta`/`AnyChange`/`Enter`/`Exit` in
//! `pydoover/tags/__init__.py`).
//!
//! Each trigger encodes one auto-logging rule. On every single-tag set
//! ([`TagsRuntime::set_tag`](super::TagsRuntime::set_tag)) every registered
//! trigger for that tag path is evaluated — so its private state stays
//! consistent — and the write is promoted to `log=true` if any fired
//! (pydoover `Tags._set_tag_value`). A fired trigger therefore lands in the
//! runtime's *immediate-log* bucket and becomes a `tag_values` channel
//! message at the end of the current loop iteration
//! ([`flush_immediate_logs`](super::TagsRuntime::flush_immediate_logs)),
//! with any pending periodic-log entry for the same paths deduped — exactly
//! pydoover `TagsManagerDocker.set_tags(log=True)`.
//!
//! `prev` semantics mirror pydoover: the value the manager currently holds
//! for the tag, falling back to the tag's *declared default*, else "not
//! set" (`None` here, `NotSet` in pydoover).

use serde_json::Value;

/// Which crossings fire (pydoover `_Crossing._DIRECTIONS`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CrossDirection {
    Both,
    Rise,
    Fall,
}

/// One `log_on=` rule (a pydoover `_LogTrigger` descriptor).
///
/// Numeric tags (`number`/`integer`) accept the crossing and delta
/// triggers; boolean/string tags accept [`AnyChange`](Self::any_change) /
/// [`Enter`](Self::enter) / [`Exit`](Self::exit) — validated by
/// [`Tag::with_log_on`](super::Tag::with_log_on) like pydoover's
/// `_allowed_log_on_for_type`.
#[derive(Debug, Clone, PartialEq)]
pub enum LogTrigger {
    /// Log on crossing any threshold in either direction (pydoover
    /// `Cross`). `deadband` is a hysteresis band: crossings only fire when
    /// the value moves at least `deadband / 2` beyond the threshold.
    Cross { thresholds: Vec<f64>, deadband: f64 },
    /// Log only on rising crossings (pydoover `Rise`).
    Rise { thresholds: Vec<f64>, deadband: f64 },
    /// Log only on falling crossings (pydoover `Fall`).
    Fall { thresholds: Vec<f64>, deadband: f64 },
    /// Log when the value moves at least this far from the last value this
    /// trigger fired on (pydoover `Delta(amount=…)`). The first set fires
    /// unconditionally and seeds the baseline.
    DeltaAmount(f64),
    /// Like [`DeltaAmount`](Self::DeltaAmount) but as a percentage of the
    /// baseline's magnitude (pydoover `Delta(percent=…)`); a zero baseline
    /// fires on any non-zero value.
    DeltaPercent(f64),
    /// Log on every value transition (pydoover `AnyChange`).
    AnyChange,
    /// Log only on entering the given value (pydoover `Enter`).
    Enter(Value),
    /// Log only on exiting the given value (pydoover `Exit`).
    Exit(Value),
}

/// Sort + validate thresholds (pydoover `_Crossing.__init__`).
fn crossing_thresholds(kind: &str, thresholds: impl IntoIterator<Item = f64>) -> Vec<f64> {
    let mut thresholds: Vec<f64> = thresholds.into_iter().collect();
    assert!(!thresholds.is_empty(), "{kind} requires at least one threshold.");
    thresholds.sort_by(|a, b| a.partial_cmp(b).expect("thresholds must not be NaN"));
    thresholds
}

/// A single threshold or a collection of them, for the crossing
/// constructors (pydoover `Cross(*thresholds)`).
pub trait IntoThresholds {
    fn into_thresholds(self) -> Vec<f64>;
}

impl IntoThresholds for f64 {
    fn into_thresholds(self) -> Vec<f64> {
        vec![self]
    }
}

impl IntoThresholds for i64 {
    fn into_thresholds(self) -> Vec<f64> {
        vec![self as f64]
    }
}

impl IntoThresholds for Vec<f64> {
    fn into_thresholds(self) -> Vec<f64> {
        self
    }
}

impl<const N: usize> IntoThresholds for [f64; N] {
    fn into_thresholds(self) -> Vec<f64> {
        self.to_vec()
    }
}

impl IntoThresholds for &[f64] {
    fn into_thresholds(self) -> Vec<f64> {
        self.to_vec()
    }
}

impl LogTrigger {
    /// pydoover `Cross(*thresholds)`. Panics on an empty threshold list
    /// (pydoover raises `ValueError`).
    pub fn cross(thresholds: impl IntoThresholds) -> Self {
        LogTrigger::Cross {
            thresholds: crossing_thresholds("Cross", thresholds.into_thresholds()),
            deadband: 0.0,
        }
    }

    /// pydoover `Rise(*thresholds)`.
    pub fn rise(thresholds: impl IntoThresholds) -> Self {
        LogTrigger::Rise {
            thresholds: crossing_thresholds("Rise", thresholds.into_thresholds()),
            deadband: 0.0,
        }
    }

    /// pydoover `Fall(*thresholds)`.
    pub fn fall(thresholds: impl IntoThresholds) -> Self {
        LogTrigger::Fall {
            thresholds: crossing_thresholds("Fall", thresholds.into_thresholds()),
            deadband: 0.0,
        }
    }

    /// Set the hysteresis band of a crossing trigger (pydoover's
    /// `deadband=` keyword). Panics on non-crossing triggers.
    pub fn deadband(mut self, deadband: f64) -> Self {
        match &mut self {
            LogTrigger::Cross { deadband: d, .. }
            | LogTrigger::Rise { deadband: d, .. }
            | LogTrigger::Fall { deadband: d, .. } => *d = deadband,
            other => panic!("deadband only applies to Cross/Rise/Fall, not {other:?}"),
        }
        self
    }

    /// pydoover `Delta(amount=…)`.
    pub fn delta_amount(amount: f64) -> Self {
        LogTrigger::DeltaAmount(amount)
    }

    /// pydoover `Delta(percent=…)`.
    pub fn delta_percent(percent: f64) -> Self {
        LogTrigger::DeltaPercent(percent)
    }

    /// pydoover `AnyChange()`.
    pub fn any_change() -> Self {
        LogTrigger::AnyChange
    }

    /// pydoover `Enter(value)`.
    pub fn enter(value: impl Into<Value>) -> Self {
        LogTrigger::Enter(value.into())
    }

    /// pydoover `Exit(value)`.
    pub fn exit(value: impl Into<Value>) -> Self {
        LogTrigger::Exit(value.into())
    }

    fn kind_name(&self) -> &'static str {
        match self {
            LogTrigger::Cross { .. } => "Cross",
            LogTrigger::Rise { .. } => "Rise",
            LogTrigger::Fall { .. } => "Fall",
            LogTrigger::DeltaAmount(_) | LogTrigger::DeltaPercent(_) => "Delta",
            LogTrigger::AnyChange => "AnyChange",
            LogTrigger::Enter(_) => "Enter",
            LogTrigger::Exit(_) => "Exit",
        }
    }

    fn is_numeric_trigger(&self) -> bool {
        matches!(
            self,
            LogTrigger::Cross { .. }
                | LogTrigger::Rise { .. }
                | LogTrigger::Fall { .. }
                | LogTrigger::DeltaAmount(_)
                | LogTrigger::DeltaPercent(_)
        )
    }

    /// Evaluate one update. `prev` is the manager's current value (with the
    /// declared-default fallback already applied); `None` is pydoover's
    /// `NotSet`. Mirrors the corresponding pydoover `evaluate` exactly.
    pub fn evaluate(&self, prev: Option<&Value>, new: &Value, state: &mut TriggerState) -> bool {
        match self {
            LogTrigger::Cross { thresholds, deadband } => {
                Self::evaluate_crossing(thresholds, *deadband, CrossDirection::Both, new, state)
            }
            LogTrigger::Rise { thresholds, deadband } => {
                Self::evaluate_crossing(thresholds, *deadband, CrossDirection::Rise, new, state)
            }
            LogTrigger::Fall { thresholds, deadband } => {
                Self::evaluate_crossing(thresholds, *deadband, CrossDirection::Fall, new, state)
            }
            LogTrigger::DeltaAmount(amount) => {
                Self::evaluate_delta(new, state, |diff, _last| diff >= *amount)
            }
            LogTrigger::DeltaPercent(percent) => Self::evaluate_delta(new, state, |diff, last| {
                if last == 0.0 {
                    // Percent change against zero is undefined; any
                    // non-zero new value is significant (pydoover).
                    diff != 0.0
                } else {
                    (diff / last.abs()) * 100.0 >= *percent
                }
            }),
            LogTrigger::AnyChange => {
                // pydoover: prev NotSet → None, fire when prev != new.
                !json_eq(prev.unwrap_or(&Value::Null), new)
            }
            LogTrigger::Enter(value) => {
                let prev = prev.unwrap_or(&Value::Null);
                !json_eq(prev, new) && json_eq(new, value)
            }
            LogTrigger::Exit(value) => {
                let prev = prev.unwrap_or(&Value::Null);
                !json_eq(prev, new) && json_eq(prev, value)
            }
        }
    }

    /// pydoover `_Crossing.evaluate`: `prev` is unused — the recorded side
    /// per threshold is the source of truth.
    fn evaluate_crossing(
        thresholds: &[f64],
        deadband: f64,
        direction: CrossDirection,
        new: &Value,
        state: &mut TriggerState,
    ) -> bool {
        // Non-numeric values (null, bools, strings, containers) never fire.
        let Some(new) = new.as_f64() else { return false };

        // sides[i]: whether the value is "above" thresholds[i]; every
        // threshold starts "below" (pydoover `sides.get(t, "below")`).
        let sides = state.sides.get_or_insert_with(|| vec![false; thresholds.len()]);
        let half_band = deadband / 2.0;
        let mut fired = false;
        for (side, t) in sides.iter_mut().zip(thresholds) {
            let upper = t + half_band;
            let lower = t - half_band;
            if new >= upper && !*side {
                *side = true;
                if matches!(direction, CrossDirection::Both | CrossDirection::Rise) {
                    fired = true;
                }
            } else if new <= lower && *side {
                *side = false;
                if matches!(direction, CrossDirection::Both | CrossDirection::Fall) {
                    fired = true;
                }
            }
        }
        fired
    }

    /// pydoover `Delta.evaluate`: compares against the last value this
    /// trigger *fired* on; the first set fires unconditionally.
    fn evaluate_delta(
        new: &Value,
        state: &mut TriggerState,
        fires: impl FnOnce(f64, f64) -> bool,
    ) -> bool {
        let Some(new) = new.as_f64() else { return false };
        match state.last_logged {
            None => {
                state.last_logged = Some(new);
                true
            }
            Some(last) => {
                let fired = fires((new - last).abs(), last);
                if fired {
                    state.last_logged = Some(new);
                }
                fired
            }
        }
    }
}

/// JSON equality with Python's cross-representation numeric semantics
/// (`1 == 1.0`). Note Python's `True == 1` is deliberately *not* replicated
/// — pydoover's typed tags never compare bools against numbers.
fn json_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
        _ => a == b,
    }
}

/// Per-trigger mutable state (pydoover's per-descriptor `state` dict, owned
/// by the `Tags` instance).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TriggerState {
    /// Crossing triggers: whether the value is above each (sorted)
    /// threshold; `None` until first evaluated.
    sides: Option<Vec<bool>>,
    /// Delta triggers: the last value the trigger fired on.
    last_logged: Option<f64>,
}

/// Every trigger registered for one tag path, plus its state and the tag's
/// declared default (the `prev` fallback).
#[derive(Debug, Clone, PartialEq)]
pub struct TriggerSet {
    triggers: Vec<LogTrigger>,
    states: Vec<TriggerState>,
    /// The declared default: `None` == pydoover `NotSet`; `Some(Null)` == a
    /// declared `default=None`.
    default: Option<Value>,
}

impl TriggerSet {
    pub fn new(triggers: Vec<LogTrigger>, default: Option<Value>) -> Self {
        let states = vec![TriggerState::default(); triggers.len()];
        Self { triggers, states, default }
    }

    /// Evaluate one update against every trigger — all of them run so each
    /// updates its private state (pydoover `_evaluate_triggers`) — and OR
    /// the results. `current` is the manager's stored value (`None` when
    /// the tag has never been set); the declared-default fallback is
    /// applied here, mirroring pydoover `Tags._get_tag_value`.
    pub fn evaluate(&mut self, current: Option<&Value>, new: &Value) -> bool {
        let prev = current.or(self.default.as_ref());
        let mut fired = false;
        for (trigger, state) in self.triggers.iter().zip(&mut self.states) {
            if trigger.evaluate(prev, new, state) {
                fired = true;
            }
        }
        fired
    }
}

/// Validate `log_on=` triggers against a declared tag type — pydoover
/// `_allowed_log_on_for_type` + `_normalise_log_on`, which raise
/// `TypeError` at declaration time (declaration is startup-time code, so
/// this panics).
pub(crate) fn validate_log_on(tag_type: &str, triggers: &[LogTrigger]) {
    let numeric = matches!(tag_type, "number" | "integer" | "float");
    let state = matches!(tag_type, "boolean" | "string");
    assert!(
        numeric || state,
        "log_on is not supported for tag_type {tag_type:?}; \
         supported types are: number, integer, float, boolean, string."
    );
    for trigger in triggers {
        let ok = if numeric { trigger.is_numeric_trigger() } else { !trigger.is_numeric_trigger() };
        assert!(
            ok,
            "{tag_type} log_on accepts {} descriptors, got {}.",
            if numeric { "Cross, Rise, Fall, Delta" } else { "AnyChange, Enter, Exit" },
            trigger.kind_name(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Replay a value sequence like pydoover's `Tags._set_tag_value` loop:
    /// prev = stored value (or default), evaluate, then store.
    fn replay(set: &mut TriggerSet, values: &[Value]) -> Vec<bool> {
        let mut store: Option<Value> = None;
        values
            .iter()
            .map(|v| {
                let fired = set.evaluate(store.as_ref(), v);
                store = Some(v.clone());
                fired
            })
            .collect()
    }

    #[test]
    fn cross_up_then_down_both_log() {
        // pydoover test_crossing_up_then_down_both_log (Cross(100))
        let mut set = TriggerSet::new(vec![LogTrigger::cross(100.0)], None);
        assert_eq!(replay(&mut set, &[json!(80), json!(120), json!(70)]), [false, true, true]);
    }

    #[test]
    fn cross_initial_value_above_threshold_logs() {
        let mut set = TriggerSet::new(vec![LogTrigger::cross(100.0)], None);
        assert_eq!(replay(&mut set, &[json!(120)]), [true]);
    }

    #[test]
    fn cross_no_relog_while_staying_above() {
        let mut set = TriggerSet::new(vec![LogTrigger::cross(100.0)], None);
        assert_eq!(
            replay(&mut set, &[json!(120), json!(130), json!(110)]),
            [true, false, false]
        );
    }

    #[test]
    fn deadband_suppresses_oscillation() {
        // pydoover: Cross(50, 100, deadband=4) → fires up at >=52, down at <=48.
        let mut set =
            TriggerSet::new(vec![LogTrigger::cross([50.0, 100.0]).deadband(4.0)], None);
        assert_eq!(
            replay(
                &mut set,
                &[json!(40), json!(51), json!(49), json!(53), json!(49), json!(47)]
            ),
            [false, false, false, true, false, true]
        );
    }

    #[test]
    fn multiple_thresholds_fire_independently() {
        let mut set =
            TriggerSet::new(vec![LogTrigger::cross([50.0, 100.0]).deadband(4.0)], None);
        assert_eq!(
            replay(&mut set, &[json!(60), json!(110), json!(95), json!(40)]),
            [true, true, true, true]
        );
    }

    #[test]
    fn rise_only_fires_going_up() {
        let mut set = TriggerSet::new(vec![LogTrigger::rise(100.0)], None);
        assert_eq!(
            replay(&mut set, &[json!(80), json!(120), json!(80), json!(120)]),
            [false, true, false, true]
        );
    }

    #[test]
    fn fall_only_fires_going_down_and_initial_below_is_silent() {
        let mut set = TriggerSet::new(vec![LogTrigger::fall(10.0)], None);
        assert_eq!(
            replay(&mut set, &[json!(50), json!(5), json!(50), json!(5)]),
            [false, true, false, true]
        );
        let mut set = TriggerSet::new(vec![LogTrigger::fall(10.0)], None);
        assert_eq!(replay(&mut set, &[json!(5)]), [false]);
    }

    #[test]
    fn delta_amount_baseline_only_advances_on_fire() {
        let mut set = TriggerSet::new(vec![LogTrigger::delta_amount(5.0)], None);
        assert_eq!(
            replay(&mut set, &[json!(80), json!(82), json!(83), json!(85)]),
            [true, false, false, true]
        );
    }

    #[test]
    fn delta_percent_handles_zero_baseline() {
        let mut set = TriggerSet::new(vec![LogTrigger::delta_percent(10.0)], None);
        assert_eq!(
            replay(&mut set, &[json!(0), json!(0), json!(5), json!(5)]),
            [true, false, true, false]
        );
    }

    #[test]
    fn any_change_fires_each_transition_and_ignores_default_matches() {
        let mut set = TriggerSet::new(vec![LogTrigger::any_change()], None);
        assert_eq!(replay(&mut set, &[json!(true), json!(false), json!(true)]), [true, true, true]);

        // declared default participates as prev on the first set
        let mut set = TriggerSet::new(vec![LogTrigger::any_change()], Some(json!(false)));
        assert_eq!(replay(&mut set, &[json!(false), json!(true)]), [false, true]);
    }

    #[test]
    fn enter_exit_composition() {
        let mut set = TriggerSet::new(
            vec![LogTrigger::enter("error"), LogTrigger::exit("ok")],
            None,
        );
        assert_eq!(
            replay(&mut set, &[json!("ok"), json!("warn"), json!("error"), json!("warn")]),
            [false, true, true, false]
        );
    }

    #[test]
    fn non_numeric_values_never_fire_numeric_triggers() {
        let mut set = TriggerSet::new(
            vec![LogTrigger::cross(10.0), LogTrigger::delta_amount(1.0)],
            None,
        );
        assert_eq!(
            replay(&mut set, &[Value::Null, json!(true), json!("15")]),
            [false, false, false]
        );
    }

    #[test]
    fn numeric_equality_crosses_representations() {
        // Python 1 == 1.0; serde_json Number(1) != Number(1.0) by default.
        let mut set = TriggerSet::new(vec![LogTrigger::any_change()], None);
        assert_eq!(replay(&mut set, &[json!(1), json!(1.0)]), [true, false]);
    }

    #[test]
    #[should_panic(expected = "at least one threshold")]
    fn empty_thresholds_panic() {
        LogTrigger::cross(Vec::new());
    }

    #[test]
    #[should_panic(expected = "log_on accepts")]
    fn validation_rejects_mismatched_trigger() {
        validate_log_on("boolean", &[LogTrigger::delta_amount(1.0)]);
    }

    #[test]
    #[should_panic(expected = "not supported for tag_type")]
    fn validation_rejects_unknown_type() {
        validate_log_on("object", &[LogTrigger::any_change()]);
    }
}
