//! JSON form of [`v1::Trigger`] sets, used by the agent's `triggers=<file>` option and the CLI.
//!
//! ```json
//! {"triggers": [
//!   {"id": "heap-90", "threshold": {"metric": "heap.used_pct", "above": 90, "for": "10s"},
//!    "actions": [{"heap_dump": {"path": "/tmp/heap-{pid}-{time}.hprof"}}, {"thread_dump": {}}],
//!    "cooldown": "5m", "max_fires": 1},
//!   {"id": "every-minute", "timer": {"delay": "30s", "period": "1m"},
//!    "actions": [{"save_snapshot": "/tmp/app-{n}.emberlink"}]},
//!   {"id": "oom", "out_of_memory": {}, "actions": [{"thread_dump": {}}]},
//!   {"id": "exit", "jvm_exit": {}, "actions": [{"save_snapshot": "/tmp/final.emberlink"}]},
//!   {"id": "start", "jvm_start": {}, "actions": [{"stop_recording": "alloc"}]}
//! ]}
//! ```
//!
//! A bare array is accepted too. Durations are strings with a unit (`ns`, `us`, `ms`, `s`, `m`,
//! `h`) or numbers of milliseconds.

use crate::v1;
use serde_json::{Map, Value};

/// Parses a trigger set. Errors name the offending trigger.
pub fn parse_json(text: &str) -> Result<Vec<v1::Trigger>, String> {
    let v: Value = serde_json::from_str(text).map_err(|e| format!("invalid JSON: {e}"))?;
    let list = match &v {
        Value::Array(a) => a,
        Value::Object(o) => match o.get("triggers") {
            Some(Value::Array(a)) => a,
            _ => return Err("expected {\"triggers\": [...]} or an array".into()),
        },
        _ => return Err("expected {\"triggers\": [...]} or an array".into()),
    };
    list.iter()
        .enumerate()
        .map(|(i, t)| {
            let o = t.as_object().ok_or_else(|| format!("trigger {i}: expected an object"))?;
            let id = o.get("id").and_then(Value::as_str).map(String::from).unwrap_or_else(|| format!("trigger-{i}"));
            trigger(id.clone(), o).map_err(|e| format!("trigger `{id}`: {e}"))
        })
        .collect()
}

/// Milliseconds from `"10s"`, `"250ms"`, `"5m"` or a number of milliseconds.
pub fn duration_ms(v: &Value) -> Result<i64, String> {
    if let Some(n) = v.as_f64() {
        return Ok(n as i64);
    }
    let s = v.as_str().ok_or("expected a duration")?.trim();
    let i = s.find(|c: char| !c.is_ascii_digit() && c != '.').unwrap_or(s.len());
    let n: f64 = s[..i].parse().map_err(|_| format!("bad duration `{s}`"))?;
    let ms = match &s[i..] {
        "" | "ms" => n,
        "ns" => n / 1e6,
        "us" => n / 1e3,
        "s" => n * 1e3,
        "m" => n * 60e3,
        "h" => n * 3600e3,
        u => return Err(format!("bad duration unit `{u}`")),
    };
    Ok(ms as i64)
}

fn opt_ms(o: &Map<String, Value>, key: &str) -> Result<i64, String> {
    o.get(key).map(duration_ms).transpose().map(|v| v.unwrap_or(0))
}

fn trigger(id: String, o: &Map<String, Value>) -> Result<v1::Trigger, String> {
    use v1::trigger::Condition;
    let mut conditions = vec![];
    if let Some(t) = o.get("timer") {
        let t = t.as_object().ok_or("`timer` must be an object")?;
        conditions.push(Condition::Timer(v1::TimerCondition {
            delay_ms: opt_ms(t, "delay")?,
            period_ms: opt_ms(t, "period")?,
        }));
    }
    if let Some(t) = o.get("threshold") {
        let t = t.as_object().ok_or("`threshold` must be an object")?;
        let metric = t.get("metric").and_then(Value::as_str).ok_or("`threshold.metric` is required")?;
        let above = t.get("above").and_then(Value::as_f64);
        let below = t.get("below").and_then(Value::as_f64);
        if above.is_none() && below.is_none() {
            return Err("`threshold` needs `above` or `below`".into());
        }
        conditions.push(Condition::Threshold(v1::ThresholdCondition {
            metric: metric.into(),
            above,
            below,
            for_ms: opt_ms(t, "for")?,
        }));
    }
    if o.contains_key("out_of_memory") {
        conditions.push(Condition::OutOfMemory(v1::OutOfMemoryCondition {}));
    }
    if o.contains_key("jvm_start") {
        conditions.push(Condition::JvmStart(v1::JvmStartCondition {}));
    }
    if o.contains_key("jvm_exit") {
        conditions.push(Condition::JvmExit(v1::JvmExitCondition {}));
    }
    let condition = match conditions.len() {
        1 => conditions.pop(),
        0 => return Err("no condition (timer, threshold, out_of_memory, jvm_start or jvm_exit)".into()),
        _ => return Err("more than one condition".into()),
    };
    let actions = o
        .get("actions")
        .and_then(Value::as_array)
        .ok_or("`actions` must be a non-empty array")?
        .iter()
        .map(action)
        .collect::<Result<Vec<_>, _>>()?;
    if actions.is_empty() {
        return Err("`actions` must be a non-empty array".into());
    }
    Ok(v1::Trigger {
        id,
        condition,
        actions,
        cooldown_ms: opt_ms(o, "cooldown")?,
        max_fires: o.get("max_fires").and_then(Value::as_u64).unwrap_or(0) as u32,
    })
}

fn action(v: &Value) -> Result<v1::TriggerAction, String> {
    use v1::trigger_action::Kind;
    let (name, arg) = match v {
        Value::String(s) => (s.as_str(), &Value::Null),
        Value::Object(o) if o.len() == 1 => {
            let (k, v) = o.iter().next().unwrap();
            (k.as_str(), v)
        }
        _ => return Err("an action is a one-key object such as {\"thread_dump\": {}}".into()),
    };
    let path = |what: &str| -> Result<String, String> {
        arg.as_str()
            .or_else(|| arg.get("path").and_then(Value::as_str))
            .map(String::from)
            .ok_or_else(|| format!("`{what}` needs a path"))
    };
    let recording = |what: &str| -> Result<String, String> {
        match arg.as_str() {
            Some(k @ ("cpu" | "alloc" | "monitor")) => Ok(k.into()),
            _ => Err(format!("`{what}` takes \"cpu\", \"alloc\" or \"monitor\"")),
        }
    };
    let kind = match name {
        "start_recording" => Kind::StartRecording(recording(name)?),
        "stop_recording" => Kind::StopRecording(recording(name)?),
        "bookmark" => Kind::Bookmark(v1::AddBookmark {
            label: arg.as_str().or_else(|| arg.get("label").and_then(Value::as_str)).unwrap_or("").into(),
            color: arg.get("color").and_then(Value::as_str).unwrap_or("").into(),
        }),
        "save_snapshot" => Kind::SaveSnapshot(path(name)?),
        "heap_dump" => Kind::HeapDump(v1::HeapDumpAction {
            path: path(name)?,
            live_only: arg.get("live_only").and_then(Value::as_bool).unwrap_or(true),
        }),
        "thread_dump" => Kind::ThreadDump(v1::ThreadDumpRequest {}),
        "run_gc" => Kind::RunGc(v1::RunGc {}),
        other => return Err(format!("unknown action `{other}`")),
    };
    Ok(v1::TriggerAction { kind: Some(kind) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use v1::trigger::Condition;
    use v1::trigger_action::Kind;

    #[test]
    fn parses_trigger_sets() {
        let t = parse_json(
            r#"{"triggers": [
                {"id": "heap", "threshold": {"metric": "heap.used_pct", "above": 90, "for": "10s"},
                 "actions": [{"heap_dump": {"path": "/tmp/h.hprof"}}, "thread_dump"], "cooldown": "5m", "max_fires": 1},
                {"timer": {"delay": 500, "period": "1m"}, "actions": [{"save_snapshot": "/tmp/{n}.emberlink"}]},
                {"id": "x", "jvm_exit": {}, "actions": [{"stop_recording": "cpu"}, {"bookmark": {"label": "bye"}}]}
            ]}"#,
        )
        .unwrap();
        assert_eq!(t.len(), 3);
        assert_eq!(t[0].cooldown_ms, 300_000);
        assert_eq!(t[0].max_fires, 1);
        match &t[0].condition {
            Some(Condition::Threshold(c)) => {
                assert_eq!((c.above, c.below, c.for_ms), (Some(90.0), None, 10_000));
            }
            c => panic!("{c:?}"),
        }
        assert!(matches!(t[0].actions[1].kind, Some(Kind::ThreadDump(_))));
        assert_eq!(t[1].id, "trigger-1");
        assert!(matches!(t[1].condition, Some(Condition::Timer(v1::TimerCondition { delay_ms: 500, period_ms: 60_000 }))));
        assert!(matches!(&t[2].actions[0].kind, Some(Kind::StopRecording(k)) if k == "cpu"));
    }

    #[test]
    fn rejects_bad_triggers() {
        assert!(parse_json(r#"[{"actions": ["run_gc"]}]"#).unwrap_err().contains("no condition"));
        assert!(parse_json(r#"[{"jvm_start": {}, "actions": ["explode"]}]"#).unwrap_err().contains("unknown action"));
        assert!(parse_json(r#"[{"threshold": {"metric": "cpu.process"}, "actions": ["run_gc"]}]"#).is_err());
        assert!(parse_json(r#"[{"jvm_start": {}, "actions": [{"start_recording": "disk"}]}]"#).is_err());
        assert_eq!(duration_ms(&Value::from("1.5s")).unwrap(), 1500);
    }
}
