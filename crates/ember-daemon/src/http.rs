//! HTTP + WebSocket transport for the Query API (docs/spec/query-api.md).

use crate::engine::{Engine, Session};
use crate::query::*;
use axum::body::Body;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Clone)]
pub struct AppState {
    pub engine: Arc<Engine>,
    pub token: Arc<str>,
    pub ui_dir: Option<Arc<PathBuf>>,
}

pub struct ApiError(anyhow::Error);

impl<E: Into<anyhow::Error>> From<E> for ApiError {
    fn from(e: E) -> Self {
        Self(e.into())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let msg = format!("{:#}", self.0);
        let status = if msg.starts_with("no such") {
            StatusCode::NOT_FOUND
        } else if msg.contains("still loading") {
            StatusCode::CONFLICT
        } else {
            StatusCode::BAD_REQUEST
        };
        (status, Json(json!({ "error": msg }))).into_response()
    }
}

type ApiResult<T = Json<Value>> = Result<T, ApiError>;

fn to_json<T: serde::Serialize>(v: T) -> ApiResult {
    Ok(Json(serde_json::to_value(v)?))
}

/// Parses an optional JSON body (empty body = `{}`).
fn body<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, ApiError> {
    if bytes.iter().all(|b| b.is_ascii_whitespace()) || bytes.trim_ascii() == b"null" {
        return Ok(serde_json::from_value(json!({}))?);
    }
    serde_json::from_slice(bytes).map_err(|e| ApiError(anyhow::anyhow!("invalid request body: {e}")))
}

/// Runs a CPU-bound query off the async executor.
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> anyhow::Result<T> + Send + 'static,
) -> Result<T, ApiError> {
    tokio::task::spawn_blocking(f).await.map_err(|e| ApiError(e.into()))?.map_err(ApiError)
}

fn session(st: &AppState, id: &str) -> Result<Arc<Session>, ApiError> {
    Ok(st.engine.session(id)?)
}

pub fn router(state: AppState) -> Router {
    let api = Router::new()
        .route("/sessions", get(list_sessions))
        .route("/sessions/open", post(open))
        .route("/sessions/attach", post(attach))
        .route("/sessions/demo", post(demo))
        .route("/sessions/{id}", axum::routing::delete(close))
        .route("/sessions/{id}/summary", get(summary_h))
        .route("/sessions/{id}/telemetry", post(telemetry_h))
        .route("/sessions/{id}/tree", post(tree_h))
        .route("/sessions/{id}/hotspots", post(hotspots_h))
        .route("/sessions/{id}/flame", post(flame_h))
        .route("/sessions/{id}/callgraph", post(callgraph_h))
        .route("/sessions/{id}/histogram", post(histogram_h))
        .route("/sessions/{id}/threads", get(threads_h))
        .route("/sessions/{id}/thread-states", post(thread_states_h))
        .route("/sessions/{id}/thread-dumps", get(thread_dumps_h).post(live_h))
        .route("/sessions/{id}/monitors", post(monitors_h))
        .route("/sessions/{id}/insights", get(insights_h))
        .route("/sessions/{id}/events", post(events_h))
        .route("/sessions/{id}/export", post(export_h))
        .route("/sessions/{id}/heap/{*rest}", any(heap_h))
        .route("/sessions/{id}/recording", post(live_h))
        .route("/sessions/{id}/heap-dump", post(live_h))
        .route("/sessions/{id}/snapshot", post(live_h))
        .route("/sessions/{id}/bookmark", post(live_h))
        .route("/sessions/{id}/gc", post(live_h))
        .route("/compare", post(compare_h))
        .route("/compare/flame", post(compare_flame_h))
        .route("/jvms", get(jvms))
        .route("/remote/discover", post(remote_discover))
        .route("/events", get(events))
        .route("/ide", get(ide_ws))
        .route("/ide/navigate", post(ide_navigate))
        .route("/version", get(|| async { Json(json!({ "version": env!("CARGO_PKG_VERSION"), "api": 1 })) }))
        .layer(middleware::from_fn_with_state(state.clone(), auth));
    Router::new()
        .nest("/api/v1", api)
        .fallback(get(static_ui))
        .layer(
            tower_http::cors::CorsLayer::new()
                .allow_origin(tower_http::cors::AllowOrigin::predicate(|o: &HeaderValue, _| {
                    let o = o.as_bytes();
                    o.starts_with(b"http://localhost:")
                        || o.starts_with(b"http://127.0.0.1:")
                        || o.starts_with(b"tauri://")
                        || o.starts_with(b"http://tauri.localhost")
                }))
                .allow_methods([Method::GET, Method::POST, Method::DELETE])
                .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE]),
        )
        .with_state(state)
}

#[derive(Deserialize)]
struct TokenQuery {
    token: Option<String>,
}

fn token_ok(st: &AppState, headers: &HeaderMap, query: Option<&str>) -> bool {
    let want = st.token.as_bytes();
    let from_header = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|t| t.as_bytes().to_vec());
    let from_query = query.and_then(serde_urlencoded_token).map(|t| t.into_bytes());
    [from_header, from_query].into_iter().flatten().any(|t| constant_time_eq(&t, want))
}

fn serde_urlencoded_token(q: &str) -> Option<String> {
    q.split('&').find_map(|kv| kv.strip_prefix("token=")).map(|t| t.replace("%2D", "-"))
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn auth(State(st): State<AppState>, req: Request, next: Next) -> Response {
    if req.method() == Method::OPTIONS || token_ok(&st, req.headers(), req.uri().query()) {
        return next.run(req).await;
    }
    (StatusCode::UNAUTHORIZED, Json(json!({ "error": "missing or invalid token" }))).into_response()
}

async fn list_sessions(State(st): State<AppState>) -> ApiResult {
    to_json(st.engine.sessions().iter().map(|s| s.info()).collect::<Vec<_>>())
}

#[derive(Deserialize)]
struct OpenReq {
    path: PathBuf,
}

async fn open(State(st): State<AppState>, b: axum::body::Bytes) -> ApiResult {
    let r: OpenReq = body(&b)?;
    to_json(st.engine.open(&r.path)?.info())
}

async fn demo(State(st): State<AppState>) -> ApiResult {
    let s = blocking(|| Ok(crate::demo::demo_profile())).await?;
    to_json(st.engine.add_profile(crate::engine::SessionKind::Ember, "Demo shop (synthetic)", s).info())
}

async fn attach(State(st): State<AppState>, b: axum::body::Bytes) -> ApiResult {
    let r: crate::live::AttachRequest = body(&b)?;
    let engine = st.engine.clone();
    to_json(crate::live::attach(&engine, r).await?.info())
}

async fn remote_discover(b: axum::body::Bytes) -> ApiResult {
    let r: Value = body(&b)?;
    Ok(Json(crate::live::remote_discover(r).await?))
}

async fn jvms() -> ApiResult {
    to_json(blocking(crate::live::list_jvms).await?)
}

async fn close(State(st): State<AppState>, Path(id): Path<String>) -> ApiResult {
    st.engine.close(&id)?;
    Ok(Json(json!({})))
}

async fn summary_h(State(st): State<AppState>, Path(id): Path<String>) -> ApiResult {
    let s = session(&st, &id)?;
    to_json(blocking(move || summary(&s)).await?)
}

macro_rules! post_query {
    ($name:ident, $req:ty, $f:expr) => {
        async fn $name(
            State(st): State<AppState>,
            Path(id): Path<String>,
            b: axum::body::Bytes,
        ) -> ApiResult {
            let s = session(&st, &id)?;
            let r: $req = body(&b)?;
            to_json(blocking(move || $f(&s, &r)).await?)
        }
    };
}

post_query!(telemetry_h, TelemetryRequest, telemetry);
post_query!(tree_h, TreeRequest, tree);
post_query!(hotspots_h, HotSpotsRequest, hotspots);
post_query!(flame_h, TreeRequest, flame);
post_query!(callgraph_h, CallGraphRequest, callgraph);
post_query!(histogram_h, HistogramRequest, histogram);
post_query!(thread_states_h, ThreadStatesRequest, thread_states);
post_query!(monitors_h, MonitorsRequest, monitors);
post_query!(events_h, crate::events::EventsRequest, crate::events::events);

async fn threads_h(State(st): State<AppState>, Path(id): Path<String>) -> ApiResult {
    let s = session(&st, &id)?;
    to_json(blocking(move || threads(&s)).await?)
}

async fn thread_dumps_h(State(st): State<AppState>, Path(id): Path<String>) -> ApiResult {
    let s = session(&st, &id)?;
    to_json(blocking(move || thread_dumps(&s)).await?)
}

async fn insights_h(State(st): State<AppState>, Path(id): Path<String>) -> ApiResult {
    let s = session(&st, &id)?;
    let v = blocking(move || crate::insights::session_insights(&s)).await?;
    to_json(json!({ "insights": v }))
}

async fn export_h(
    State(st): State<AppState>,
    Path(id): Path<String>,
    b: axum::body::Bytes,
) -> Result<Response, ApiError> {
    let s = session(&st, &id)?;
    let r: crate::export::ExportRequest = body(&b)?;
    let ext = r.format.extension();
    let view = r.view.clone();
    let out = blocking(move || crate::export::export(&s, &r)).await?;
    Ok((
        [
            (header::CONTENT_TYPE, out.content_type.to_owned()),
            (header::CONTENT_DISPOSITION, format!("attachment; filename=\"{id}-{view}.{ext}\"")),
        ],
        out.body,
    )
        .into_response())
}

async fn heap_h(
    State(st): State<AppState>,
    Path((id, rest)): Path<(String, String)>,
    method: Method,
    b: axum::body::Bytes,
) -> ApiResult {
    let s = session(&st, &id)?;
    let engine = st.engine.clone();
    let req: Value = body(&b)?;
    let post = method == Method::POST;
    to_json(blocking(move || crate::heap::handle(&engine, &s, &rest, post, req)).await?)
}

async fn live_h(State(st): State<AppState>, Path(id): Path<String>, req: Request) -> ApiResult {
    let s = session(&st, &id)?;
    let action = req.uri().path().rsplit('/').next().unwrap_or_default().to_owned();
    let bytes = axum::body::to_bytes(req.into_body(), 1 << 20).await?;
    let r: Value = body(&bytes)?;
    to_json(crate::live::control(&st.engine, &s, &action, r).await?)
}

async fn compare_h(State(st): State<AppState>, b: axum::body::Bytes) -> ApiResult {
    let r: CompareRequest = body(&b)?;
    let (a, bb) = (session(&st, &r.a)?, session(&st, &r.b)?);
    if r.kind == "heap_histogram" {
        let engine = st.engine.clone();
        return to_json(
            blocking(move || {
                crate::heap::handle(&engine, &a, "histogram-diff", true, json!({ "other_session": bb.id }))
            })
            .await?,
        );
    }
    to_json(blocking(move || compare(&a, &bb, &r)).await?)
}

async fn compare_flame_h(State(st): State<AppState>, b: axum::body::Bytes) -> ApiResult {
    let r: CompareRequest = body(&b)?;
    let (a, bb) = (session(&st, &r.a)?, session(&st, &r.b)?);
    to_json(blocking(move || compare_flame(&a, &bb, &r)).await?)
}

async fn events(State(st): State<AppState>, ws: WebSocketUpgrade) -> Response {
    let rx = st.engine.subscribe();
    ws.on_upgrade(move |socket| pump_events(socket, rx))
}

async fn pump_events(
    mut socket: WebSocket,
    mut rx: tokio::sync::broadcast::Receiver<crate::engine::ServerEvent>,
) {
    use tokio::sync::broadcast::error::RecvError;
    loop {
        tokio::select! {
            ev = rx.recv() => match ev {
                Ok(ev) => {
                    let text = serde_json::to_string(&ev).unwrap_or_default();
                    if socket.send(Message::Text(text.into())).await.is_err() {
                        return;
                    }
                }
                Err(RecvError::Lagged(_)) => {
                    let _ = socket.send(Message::Text(r#"{"type":"sessions"}"#.into())).await;
                }
                Err(RecvError::Closed) => return,
            },
            msg = socket.recv() => match msg {
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return,
                _ => {}
            },
        }
    }
}

async fn ide_navigate(State(st): State<AppState>, b: axum::body::Bytes) -> ApiResult {
    let params: Value = body(&b)?;
    let delivered = st.engine.notify_ide("navigate", params);
    Ok(Json(json!({ "delivered": delivered })))
}

async fn ide_ws(State(st): State<AppState>, ws: WebSocketUpgrade) -> Response {
    let engine = st.engine.clone();
    ws.on_upgrade(move |socket| ide_session(socket, engine))
}

async fn ide_session(mut socket: WebSocket, engine: Arc<Engine>) {
    use tokio::sync::broadcast::error::RecvError;
    let mut rx = engine.subscribe_ide();
    loop {
        tokio::select! {
            ev = rx.recv() => match ev {
                Ok(n) => {
                    if socket.send(Message::Text(n.to_string().into())).await.is_err() {
                        return;
                    }
                }
                Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => return,
            },
            msg = socket.recv() => match msg {
                Some(Ok(Message::Text(t))) => {
                    let Ok(req) = serde_json::from_str::<Value>(&t) else { continue };
                    let e = engine.clone();
                    let resp = tokio::task::spawn_blocking(move || crate::ide::handle(&e, &req)).await.ok().flatten();
                    if let Some(r) = resp
                        && socket.send(Message::Text(r.to_string().into())).await.is_err()
                    {
                        return;
                    }
                }
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return,
                _ => {}
            },
        }
    }
}

/// Serves the built UI in web mode. `index.html` gets the API token injected only when the
/// request itself carries the token (`/?token=…`), so the page can't be used to learn it.
async fn static_ui(
    State(st): State<AppState>,
    Query(q): Query<TokenQuery>,
    headers: HeaderMap,
    uri: axum::http::Uri,
) -> Response {
    let Some(dir) = st.ui_dir.clone() else {
        return (StatusCode::NOT_FOUND, "Ember daemon: UI not enabled (start with --web)").into_response();
    };
    let rel = uri.path().trim_start_matches('/');
    let candidate = dir.join(rel);
    let is_file = !rel.is_empty()
        && !rel.split('/').any(|c| c == "..")
        && tokio::fs::metadata(&candidate).await.map(|m| m.is_file()).unwrap_or(false);
    if is_file {
        let bytes = match tokio::fs::read(&candidate).await {
            Ok(b) => b,
            Err(_) => return StatusCode::NOT_FOUND.into_response(),
        };
        let ct = match candidate.extension().and_then(|e| e.to_str()) {
            Some("js" | "mjs") => "text/javascript",
            Some("css") => "text/css",
            Some("html") => "text/html; charset=utf-8",
            Some("svg") => "image/svg+xml",
            Some("png") => "image/png",
            Some("woff2") => "font/woff2",
            Some("json") => "application/json",
            Some("wasm") => "application/wasm",
            _ => "application/octet-stream",
        };
        return ([(header::CONTENT_TYPE, ct)], bytes).into_response();
    }
    let index = match tokio::fs::read_to_string(dir.join("index.html")).await {
        Ok(s) => s,
        Err(_) => return (StatusCode::NOT_FOUND, "index.html not found in UI dir").into_response(),
    };
    let authed = q.token.as_deref().is_some_and(|t| constant_time_eq(t.as_bytes(), st.token.as_bytes()))
        || token_ok(&st, &headers, None);
    let html = if authed {
        let inject = format!(
            "<script>window.__EMBER__={{url:location.origin,token:{}}}</script>",
            serde_json::to_string(&*st.token).unwrap_or_default()
        );
        index.replacen("<head>", &format!("<head>{inject}"), 1)
    } else {
        index
    };
    Response::builder()
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .header(header::CACHE_CONTROL, "no-store")
        .body(Body::from(html))
        .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    async fn call(
        app: &Router,
        method: &str,
        uri: &str,
        token: Option<&str>,
        body: Value,
    ) -> (StatusCode, Value) {
        let mut req = Request::builder().method(method).uri(uri).header("content-type", "application/json");
        if let Some(t) = token {
            req = req.header("authorization", format!("Bearer {t}"));
        }
        let resp = app.clone().oneshot(req.body(Body::from(body.to_string())).unwrap()).await.unwrap();
        let status = resp.status();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    #[tokio::test]
    async fn api_roundtrip() {
        let engine = Engine::new();
        let app = router(AppState { engine, token: "t0k".into(), ui_dir: None });
        let (s, _) = call(&app, "GET", "/api/v1/sessions", None, Value::Null).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
        let (s, v) = call(&app, "POST", "/api/v1/sessions/demo", Some("t0k"), Value::Null).await;
        assert_eq!(s, StatusCode::OK, "{v}");
        let id = v["id"].as_str().unwrap().to_owned();
        let (s, v) = call(
            &app,
            "POST",
            &format!("/api/v1/sessions/{id}/tree"),
            Some("t0k"),
            json!({"kind":"cpu","depth":2}),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{v}");
        assert!(v["nodes"].as_array().unwrap().len() > 2);
        let (s, v) = call(
            &app,
            "POST",
            &format!("/api/v1/sessions/{id}/hotspots"),
            Some("t0k"),
            json!({"kind":"cpu","filter":{"states":["runnable"]}}),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{v}");
        let (s, v) =
            call(&app, "GET", &format!("/api/v1/sessions/{id}/summary"), Some("t0k"), Value::Null).await;
        assert_eq!(s, StatusCode::OK, "{v}");
        assert_eq!(v["jvm"]["vm_vendor"], "Eclipse Adoptium");
        let (s, v) =
            call(&app, "POST", &format!("/api/v1/sessions/{id}/telemetry"), Some("t0k"), Value::Null).await;
        assert_eq!(s, StatusCode::OK, "{v}");
        let (s, _) = call(&app, "GET", "/api/v1/sessions/nope/summary", Some("t0k"), Value::Null).await;
        assert_eq!(s, StatusCode::NOT_FOUND);
    }
}
