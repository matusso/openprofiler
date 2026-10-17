//! Read-only share links: scoped tokens that allow Query API reads of one session.
//!
//! A share token can read its session's views (trees, hot spots, heap, code, exports, …) and
//! nothing else: no attach or open, no recording control, dumps, triggers, MBeans or
//! instrumentation changes, no other sessions and no file browsing. Tokens live in memory, so a
//! daemon restart revokes them; closing the session makes them useless.

use parking_lot::RwLock;
use serde::Serialize;
use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Serialize)]
pub struct Share {
    pub token: String,
    pub session: String,
    /// Epoch milliseconds.
    pub created: u64,
    /// Epoch milliseconds; `None` = until revoked or the daemon stops.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires: Option<u64>,
}

/// What a request's token allows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Access {
    Full,
    /// Read-only access to one session.
    Session(String),
}

impl Access {
    pub fn sees(&self, session: &str) -> bool {
        match self {
            Access::Full => true,
            Access::Session(s) => s == session,
        }
    }
}

#[derive(Default)]
pub struct Shares(RwLock<HashMap<String, Share>>);

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

impl Shares {
    pub fn create(&self, session: &str, ttl: Option<Duration>) -> Share {
        let created = now_ms();
        let share = Share {
            token: crate::random_token(),
            session: session.to_owned(),
            created,
            expires: ttl.map(|t| created + t.as_millis() as u64),
        };
        self.0.write().insert(share.token.clone(), share.clone());
        share
    }

    /// The session a live (unexpired) share token grants.
    pub fn session_for(&self, token: &str) -> Option<String> {
        let now = now_ms();
        let map = self.0.read();
        // Compare every token in constant time rather than probing the map with the candidate.
        map.values()
            .filter(|s| crate::http::constant_time_eq(s.token.as_bytes(), token.as_bytes()))
            .find(|s| s.expires.is_none_or(|e| e > now))
            .map(|s| s.session.clone())
    }

    /// Unexpired shares, dropping expired ones and those of sessions `exists` rejects.
    pub fn list(&self, exists: impl Fn(&str) -> bool) -> Vec<Share> {
        let now = now_ms();
        let mut map = self.0.write();
        map.retain(|_, s| s.expires.is_none_or(|e| e > now) && exists(&s.session));
        let mut v: Vec<Share> = map.values().cloned().collect();
        v.sort_by_key(|s| s.created);
        v
    }

    pub fn revoke(&self, token: &str) -> bool {
        self.0.write().remove(token).is_some()
    }
}

/// Whether a share token may make this request. `path` is relative to `/api/v1`.
pub fn allowed(session: &str, method: &axum::http::Method, path: &str) -> bool {
    use axum::http::Method;
    let get = method == Method::GET;
    match path {
        "/version" | "/sessions" | "/events" => return get,
        _ => {}
    }
    let Some(rest) = path.strip_prefix("/sessions/") else { return false };
    let Some((id, action)) = rest.split_once('/') else { return false };
    if id != session {
        return false;
    }
    let (action, sub) = action.split_once('/').unwrap_or((action, ""));
    match action {
        "summary" | "threads" | "insights" | "thread-dumps" => get,
        "telemetry" | "tree" | "hotspots" | "flame" | "callgraph" | "histogram" | "thread-states"
        | "monitors" | "probes" | "events" | "export" => method == Method::POST,
        // The heap diff reads a second session.
        "heap" => sub != "histogram-diff",
        "code" => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Method;

    #[test]
    fn scope() {
        let ok = |m: Method, p: &str| allowed("s1", &m, p);
        assert!(ok(Method::GET, "/sessions"));
        assert!(ok(Method::GET, "/sessions/s1/summary"));
        assert!(ok(Method::POST, "/sessions/s1/tree"));
        assert!(ok(Method::POST, "/sessions/s1/heap/objects"));
        assert!(ok(Method::POST, "/sessions/s1/code/class"));
        assert!(ok(Method::GET, "/sessions/s1/thread-dumps"));
        assert!(!ok(Method::POST, "/sessions/s1/thread-dumps"));
        assert!(!ok(Method::POST, "/sessions/s1/heap/histogram-diff"));
        assert!(!ok(Method::POST, "/sessions/s2/tree"));
        assert!(!ok(Method::DELETE, "/sessions/s1"));
        for p in [
            "/sessions/attach",
            "/sessions/open",
            "/sessions/demo",
            "/fs/list",
            "/jvms",
            "/remote/discover",
            "/compare",
            "/ide/navigate",
            "/shares",
            "/sessions/s1/share",
            "/sessions/s1/recording",
            "/sessions/s1/heap-dump",
            "/sessions/s1/snapshot",
            "/sessions/s1/gc",
            "/sessions/s1/triggers",
            "/sessions/s1/mbeans",
            "/sessions/s1/mbean-invoke",
            "/sessions/s1/instrumentation",
            "/sessions/s1/bookmark",
        ] {
            assert!(!ok(Method::POST, p) && !ok(Method::GET, p), "{p}");
        }
    }

    #[test]
    fn tokens() {
        let shares = Shares::default();
        let a = shares.create("s1", None);
        let b = shares.create("s2", Some(Duration::ZERO));
        assert_eq!(shares.session_for(&a.token).as_deref(), Some("s1"));
        assert_eq!(shares.session_for(&b.token), None, "expired");
        assert_eq!(shares.session_for("nope"), None);
        assert_eq!(shares.list(|_| true).len(), 1);
        assert!(shares.revoke(&a.token));
        assert_eq!(shares.session_for(&a.token), None);
    }
}
