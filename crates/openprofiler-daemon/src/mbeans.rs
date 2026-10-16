//! MBean browser over the agent link: Query API JSON for `mbean-servers`, `mbean-connect`,
//! `mbeans`, `mbean`, `mbean-invoke`, `mbean-set`, `mbean-track`, `mbean-subscribe` and
//! `mbean-notifications` (see `docs/spec/query-api.md`).

use anyhow::{Result, anyhow, bail};
use openprofiler_link::LinkClient;
use openprofiler_proto::v1;
use serde_json::{Value, json};

const UNITS: [&str; 5] = ["nanos", "bytes", "count", "percent", "per_second"];

pub async fn handle(c: &LinkClient, action: &str, req: &Value) -> Result<Value> {
    let name = || req["name"].as_str().filter(|s| !s.is_empty()).ok_or_else(|| anyhow!("missing name"));
    let server = req["server"].as_str().unwrap_or("").to_string();
    match action {
        "mbean-servers" => Ok(json!({ "servers": servers(&c.mbean_servers().await?) })),
        "mbean-connect" => {
            let r = v1::MBeanConnectRequest {
                url: req["url"]
                    .as_str()
                    .filter(|s| !s.trim().is_empty())
                    .ok_or_else(|| anyhow!("missing url"))?
                    .into(),
                username: req["username"].as_str().unwrap_or("").into(),
                password: req["password"].as_str().unwrap_or("").into(),
                enabled: req["enabled"].as_bool().unwrap_or(true),
            };
            Ok(json!({ "servers": servers(&c.mbean_connect(r).await?) }))
        }
        "mbeans" => {
            let l = c.mbean_list(&server, req["pattern"].as_str().unwrap_or("")).await?;
            Ok(json!({
                "beans": l.beans.iter().map(|b| json!({
                    "name": b.name,
                    "domain": b.name.split_once(':').map_or("", |(d, _)| d),
                    "class_name": b.class_name,
                    "description": b.description,
                })).collect::<Vec<_>>(),
                "tracked": tracked(&l.tracked),
            }))
        }
        "mbean" => {
            let i = c.mbean_get(&server, name()?).await?;
            Ok(json!({
                "name": i.name,
                "class_name": i.class_name,
                "description": i.description,
                "attributes": i.attributes.iter().map(|a| json!({
                    "name": a.name,
                    "type": a.r#type,
                    "description": a.description,
                    "readable": a.readable,
                    "writable": a.writable,
                    "value": parse_value(&a.value_json),
                    "error": (!a.error.is_empty()).then_some(&a.error),
                })).collect::<Vec<_>>(),
                "operations": i.operations.iter().map(|o| json!({
                    "name": o.name,
                    "return_type": o.return_type,
                    "description": o.description,
                    "impact": o.impact,
                    "params": o.params.iter().map(|p| json!({
                        "name": p.name, "type": p.r#type, "description": p.description,
                    })).collect::<Vec<_>>(),
                })).collect::<Vec<_>>(),
                "notifications": i.notifications.iter().map(|n| json!({
                    "name": n.name, "description": n.description, "types": n.types,
                })).collect::<Vec<_>>(),
            }))
        }
        "mbean-invoke" => {
            let strings = |k: &str| -> Vec<String> {
                req[k].as_array().map(|a| a.iter().map(arg).collect()).unwrap_or_default()
            };
            let r = v1::MBeanInvokeRequest {
                name: name()?.into(),
                operation: req["operation"].as_str().ok_or_else(|| anyhow!("missing operation"))?.into(),
                signature: strings("signature"),
                args: strings("args"),
                server,
            };
            let v = c.mbean_invoke(r).await?;
            Ok(json!({ "value": parse_value(&v) }))
        }
        "mbean-set" => {
            let r = v1::MBeanSetRequest {
                name: name()?.into(),
                attribute: req["attribute"].as_str().ok_or_else(|| anyhow!("missing attribute"))?.into(),
                value: arg(&req["value"]),
                server,
            };
            c.mbean_set(r).await?;
            Ok(json!({}))
        }
        "mbean-track" => {
            let unit = match req["unit"].as_str() {
                None => 2,
                Some(u) => {
                    UNITS.iter().position(|x| *x == u).ok_or_else(|| anyhow!("unknown unit {u}"))? as u32
                }
            };
            let r = v1::MBeanTrackRequest {
                name: name()?.into(),
                attribute: req["attribute"].as_str().ok_or_else(|| anyhow!("missing attribute"))?.into(),
                item: req["item"].as_str().unwrap_or("").into(),
                label: req["label"].as_str().unwrap_or("").into(),
                unit,
                enabled: req["enabled"].as_bool().unwrap_or(true),
                server,
            };
            Ok(json!({ "tracked": tracked(&c.mbean_track(r).await?) }))
        }
        "mbean-subscribe" => {
            let r = v1::MBeanSubscribeRequest {
                server,
                name: name()?.into(),
                enabled: req["enabled"].as_bool().unwrap_or(true),
            };
            Ok(json!({ "subscriptions": subscriptions(&c.mbean_subscribe(r).await?) }))
        }
        "mbean-notifications" => {
            let n = c.mbean_notifications(req["after"].as_u64().unwrap_or(0)).await?;
            Ok(json!({
                "notifications": n.notifications.iter().map(|n| json!({
                    "seq": n.seq,
                    "time": n.time,
                    "server": n.server,
                    "name": n.source,
                    "type": n.r#type,
                    "sequence_number": n.sequence_number,
                    "message": n.message,
                    "data": parse_value(&n.data_json),
                })).collect::<Vec<_>>(),
                "subscriptions": subscriptions(&n.subscriptions),
                "last": n.last_seq,
                "missed": n.missed,
            }))
        }
        other => bail!("unknown MBean action: {other}"),
    }
}

fn servers(s: &[v1::MBeanServerInfo]) -> Vec<Value> {
    s.iter()
        .map(|s| {
            json!({
                "id": s.id,
                "kind": s.kind,
                "label": s.label,
                "default_domain": s.default_domain,
                "beans": s.bean_count,
                "error": (!s.error.is_empty()).then_some(&s.error),
            })
        })
        .collect()
}

fn subscriptions(s: &[v1::MBeanSubscription]) -> Vec<Value> {
    s.iter().map(|s| json!({ "server": s.server, "name": s.name })).collect()
}

fn tracked(t: &[v1::TrackedAttribute]) -> Vec<Value> {
    t.iter()
        .map(|t| {
            json!({
                "key": t.key,
                "label": t.label,
                "unit": UNITS.get(t.unit as usize).copied().unwrap_or("count"),
                "name": t.mbean,
                "attribute": t.attribute,
                "item": (!t.item.is_empty()).then_some(&t.item),
                "server": t.server,
            })
        })
        .collect()
}

/// Values arrive as JSON; anything unparsable is kept as a string.
fn parse_value(s: &str) -> Value {
    if s.is_empty() {
        return Value::Null;
    }
    serde_json::from_str(s).unwrap_or_else(|_| Value::String(s.into()))
}

/// Invoke/set arguments are strings on the wire; other JSON values are sent as their JSON text.
fn arg(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_values_and_args() {
        assert_eq!(parse_value(r#"{"used":5}"#)["used"], 5);
        assert_eq!(parse_value("not json"), "not json");
        assert_eq!(parse_value(""), Value::Null);
        assert_eq!(arg(&json!("x")), "x");
        assert_eq!(arg(&json!(3)), "3");
        assert_eq!(arg(&json!(["a", "b"])), r#"["a","b"]"#);
    }
}
