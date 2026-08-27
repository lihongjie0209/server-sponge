use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use axum::extract::State;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Json};
use axum::routing::{get, post};
use axum::Router;
use log::info;
use serde::Deserialize;
use tokio_stream::wrappers::IntervalStream;
use tokio_stream::StreamExt;

use crate::log_capture;
use crate::metrics::MetricsStore;
use crate::stress::StressManager;

static DASHBOARD_HTML: &str = include_str!("dashboard.html");
const SERVER_BIND_HOST: &str = "127.0.0.1";

struct AppState {
    metrics: MetricsStore,
    stress: Mutex<StressManager>,
}

/// Handle returned by `start_server` for shutdown coordination.
pub struct ServerHandle {
    _thread: JoinHandle<()>,
}

/// Start the monitoring web server on a background thread.
pub fn start_server(
    port: u16,
    metrics: MetricsStore,
    running: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> ServerHandle {
    let handle = thread::Builder::new()
        .name("http-server".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("failed to create tokio runtime for server");

            rt.block_on(async move {
                let state = Arc::new(AppState {
                    stress: Mutex::new(StressManager::new(Some(metrics.clone()))),
                    metrics,
                });

                let app = Router::new()
                    .route("/", get(index_handler))
                    .route("/api/metrics", get(metrics_handler))
                    .route("/api/events", get(sse_handler))
                    .route("/api/stress/cpu/start", post(start_cpu_handler))
                    .route("/api/stress/cpu/stop", post(stop_cpu_handler))
                    .route("/api/stress/mem/start", post(start_mem_handler))
                    .route("/api/stress/mem/stop", post(stop_mem_handler))
                    .route("/api/stress/stop", post(stop_all_handler))
                    .with_state(state);

                let addr = format!("{}:{}", SERVER_BIND_HOST, port);
                let listener = match tokio::net::TcpListener::bind(&addr).await {
                    Ok(l) => l,
                    Err(e) => {
                        log::error!("Server: failed to bind {}: {}", addr, e);
                        return;
                    }
                };

                info!("📊 监控面板已启动: http://{}:{}", SERVER_BIND_HOST, port);

                let shutdown = async move {
                    while running.load(std::sync::atomic::Ordering::Relaxed) {
                        tokio::time::sleep(Duration::from_millis(500)).await;
                    }
                };

                axum::serve(listener, app)
                    .with_graceful_shutdown(shutdown)
                    .await
                    .ok();

                info!("Server: shut down");
            });
        })
        .expect("failed to spawn server thread");

    ServerHandle { _thread: handle }
}

// ── Handlers ──

async fn index_handler() -> Html<&'static str> {
    Html(DASHBOARD_HTML)
}

async fn metrics_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    Json(state.metrics.snapshot())
}

/// SSE payload: metrics snapshot + incremental log entries.
#[derive(serde::Serialize)]
struct SsePayload {
    #[serde(flatten)]
    snapshot: crate::metrics::MetricsSnapshot,
    logs: Vec<log_capture::LogEntry>,
}

async fn sse_handler(
    State(state): State<Arc<AppState>>,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    let mut last_seq: u64 = 0;

    let stream =
        IntervalStream::new(tokio::time::interval(Duration::from_millis(500))).map(move |_| {
            // Refresh stress process status (detect timeout expiry)
            if let Ok(mut s) = state.stress.lock() {
                s.refresh_status();
            }

            let snapshot = state.metrics.snapshot();
            let (logs, latest) = log_capture::get_logs_since(last_seq);
            last_seq = latest;

            let payload = SsePayload { snapshot, logs };
            let json = serde_json::to_string(&payload).unwrap_or_default();
            Ok(Event::default().data(json))
        });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

#[derive(Deserialize)]
struct CpuStressReq {
    workers: Option<usize>,
    load: Option<usize>,
    timeout: Option<usize>,
}

#[derive(Deserialize)]
struct MemStressReq {
    mb: Option<usize>,
    timeout: Option<usize>,
}

async fn start_cpu_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CpuStressReq>,
) -> impl IntoResponse {
    let workers = req.workers.unwrap_or(1).clamp(1, 64);
    let load = req.load.unwrap_or(80).clamp(1, 100);
    let timeout = req.timeout.unwrap_or(0);
    if let Ok(mut s) = state.stress.lock() {
        s.start_cpu(workers, load, timeout);
    }
    Json(serde_json::json!({"ok": true, "workers": workers, "load": load, "timeout": timeout}))
}

async fn stop_cpu_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    if let Ok(mut s) = state.stress.lock() {
        s.stop_cpu();
    }
    Json(serde_json::json!({"ok": true}))
}

async fn start_mem_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<MemStressReq>,
) -> impl IntoResponse {
    let mb = req.mb.unwrap_or(100).clamp(1, 10000);
    let timeout = req.timeout.unwrap_or(0);
    if let Ok(mut s) = state.stress.lock() {
        s.start_mem(mb, timeout);
    }
    Json(serde_json::json!({"ok": true, "mb": mb, "timeout": timeout}))
}

async fn stop_mem_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    if let Ok(mut s) = state.stress.lock() {
        s.stop_mem();
    }
    Json(serde_json::json!({"ok": true}))
}

async fn stop_all_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    if let Ok(mut s) = state.stress.lock() {
        s.stop_all();
    }
    Json(serde_json::json!({"ok": true}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dashboard_html_not_empty() {
        assert!(DASHBOARD_HTML.len() > 100);
        assert!(DASHBOARD_HTML.contains("Server Sponge"));
    }

    #[test]
    fn test_dashboard_contains_sse_endpoint() {
        assert!(DASHBOARD_HTML.contains("/api/events"));
    }

    #[test]
    fn test_dashboard_contains_stress_endpoints() {
        assert!(DASHBOARD_HTML.contains("/api/stress"));
    }

    #[test]
    fn test_server_binds_to_loopback_by_default() {
        assert_eq!(SERVER_BIND_HOST, "127.0.0.1");
    }
}
