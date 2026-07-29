//! Local HTTP control API for CI / smoke-test orchestration.
//!
//! phone-control is a macOS GUI app (Tauri), but a Jenkins agent drives it over
//! plain HTTP: this axum server runs inside the same Tokio runtime as the video
//! WebSocket hub (see `ws.rs`) and exposes device scheduling + install + capture
//! so a `curl` from a CI shell can orchestrate a smoke run.
//!
//! ## Scope boundary (deliberate)
//! phone-control does the hardware-adjacent work it already owns — **device
//! scheduling, package install, artifact capture** — and nothing else. It does
//! **NOT** drive the app UI: Maestro/Appium own that. See
//! `docs/smoke-test-integration.md` for the full rationale.
//!
//! Because of that split, `acquire` tears down any live scrcpy stream/control
//! socket on the leased devices (`stop_stream_loop(force=true)`), so the app
//! never contends with Maestro for a device's input channel.
//!
//! Endpoints (all under `127.0.0.1:9090`):
//!   GET  /api/v1/health
//!   GET  /api/v1/devices                 — online devices + who leased them
//!   POST /api/v1/devices/acquire         — lease idle devices to a task
//!   POST /api/v1/devices/release         — release a task's leases
//!   POST /api/v1/install                 — adb install -r across leased devices
//!   POST /api/v1/capture/start|stop      — TODO Phase 2 (screenrecord + logcat)
//!   GET  /api/v1/smoke/report            — TODO Phase 2 (JUnit/JSON bundle)

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex as StdMutex};

use axum::{
    extract::{Query, State},
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::{Mutex, Semaphore};

use crate::adb::commands::{install_apk, CommandResult, DeviceRef};
use crate::adb::device::{parse_adb_devices, server_args};
use crate::adb::server::AdbServer;
use crate::adb::stream::{stop_stream_loop, ControlSockets, StreamTokens};

/// A device reserved for a running smoke task.
#[derive(Debug, Clone, Serialize)]
pub struct Lease {
    pub serial: String,
    pub server_host: String,
    pub server_port: u16,
    pub task_id: String,
}

/// Shared handles the API borrows from `AppState`, plus its own lease registry.
#[derive(Clone)]
pub struct ControlApiState {
    pub servers: Arc<Mutex<Vec<AdbServer>>>,
    pub adb_semaphore: Arc<Semaphore>,
    pub stream_tokens: StreamTokens,
    pub control_sockets: ControlSockets,
    pub leases: Arc<StdMutex<HashMap<String, Lease>>>,
}

impl ControlApiState {
    pub fn new(
        servers: Arc<Mutex<Vec<AdbServer>>>,
        adb_semaphore: Arc<Semaphore>,
        stream_tokens: StreamTokens,
        control_sockets: ControlSockets,
    ) -> Self {
        Self {
            servers,
            adb_semaphore,
            stream_tokens,
            control_sockets,
            leases: Arc::new(StdMutex::new(HashMap::new())),
        }
    }
}

/// Start the control API. Spawned from `lib.rs::setup`, alongside the WS hub.
pub async fn run_control_api(state: ControlApiState, addr: SocketAddr) -> Result<(), String> {
    let app = Router::new()
        .route("/api/v1/health", get(health))
        .route("/api/v1/devices", get(list_devices))
        .route("/api/v1/devices/acquire", post(acquire_devices))
        .route("/api/v1/devices/release", post(release_devices))
        .route("/api/v1/install", post(install))
        .route("/api/v1/capture/start", post(capture_start))
        .route("/api/v1/capture/stop", post(capture_stop))
        .route("/api/v1/smoke/report", get(report))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| format!("control-api bind {addr}: {e}"))?;
    println!("[CTRL-API] listening on http://{addr}");
    axum::serve(listener, app)
        .await
        .map_err(|e| format!("control-api serve: {e}"))
}

// ── Device discovery ──────────────────────────────────────────────────────────

/// Live-query `adb devices` on every enabled server and return the online ones.
/// Kept live (not cached) so a CI run always sees the true current fleet.
async fn online_devices(servers: &Arc<Mutex<Vec<AdbServer>>>) -> Vec<DeviceRef> {
    let snapshot: Vec<AdbServer> = { servers.lock().await.clone() };
    let mut out = Vec::new();
    for srv in snapshot.into_iter().filter(|s| s.enabled) {
        let mut args = server_args(&srv.host, srv.port);
        args.push("devices".into());
        let output = tokio::task::spawn_blocking(move || {
            std::process::Command::new(crate::adb::binaries::adb())
                .args(&args)
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
                .unwrap_or_default()
        })
        .await
        .unwrap_or_default();

        for (serial, status) in parse_adb_devices(&output) {
            // `adb devices` prints "device" for a healthy device.
            if status == "device" {
                out.push(DeviceRef {
                    serial,
                    server_host: srv.host.clone(),
                    server_port: srv.port,
                });
            }
        }
    }
    out
}

// ── Handlers ────────────────────────────────────────────────────────────────

async fn health() -> Json<Value> {
    Json(json!({ "status": "ok", "service": "phone-control control-api" }))
}

async fn list_devices(State(st): State<ControlApiState>) -> Json<Value> {
    let devices = online_devices(&st.servers).await;
    let leased = st.leases.lock().unwrap().clone();
    let list: Vec<Value> = devices
        .into_iter()
        .map(|d| {
            json!({
                "serial": d.serial,
                "server_host": d.server_host,
                "server_port": d.server_port,
                "leased_by": leased.get(&d.serial).map(|l| l.task_id.clone()),
            })
        })
        .collect();
    Json(json!({ "devices": list }))
}

fn default_count() -> usize {
    1
}

#[derive(Deserialize)]
struct AcquireReq {
    task_id: String,
    #[serde(default = "default_count")]
    count: usize,
    /// Optional explicit serials; when empty, pick any idle devices.
    #[serde(default)]
    serials: Vec<String>,
}

#[derive(Serialize)]
struct AcquireResp {
    task_id: String,
    devices: Vec<Lease>,
}

async fn acquire_devices(
    State(st): State<ControlApiState>,
    Json(req): Json<AcquireReq>,
) -> Result<Json<AcquireResp>, (StatusCode, String)> {
    let available = online_devices(&st.servers).await;
    let already_leased: HashSet<String> =
        { st.leases.lock().unwrap().keys().cloned().collect() };

    let mut chosen: Vec<DeviceRef> = Vec::new();
    for d in available {
        if already_leased.contains(&d.serial) {
            continue;
        }
        if req.serials.is_empty() {
            chosen.push(d);
            if chosen.len() >= req.count {
                break;
            }
        } else if req.serials.contains(&d.serial) {
            chosen.push(d);
        }
    }

    if chosen.is_empty() {
        return Err((
            StatusCode::CONFLICT,
            "no idle devices available for lease".into(),
        ));
    }

    // Decision #3: phone-control must not hold a device while Maestro drives it.
    // Free the scrcpy stream/control socket before handing the serial to CI.
    for d in &chosen {
        stop_stream_loop(
            Arc::clone(&st.stream_tokens),
            Arc::clone(&st.control_sockets),
            &d.serial,
            None,
            None,
            None,
            true,
        )
        .await;
    }

    let mut leases = Vec::with_capacity(chosen.len());
    {
        let mut map = st.leases.lock().unwrap();
        for d in chosen {
            let lease = Lease {
                serial: d.serial.clone(),
                server_host: d.server_host,
                server_port: d.server_port,
                task_id: req.task_id.clone(),
            };
            map.insert(d.serial.clone(), lease.clone());
            leases.push(lease);
        }
    }

    Ok(Json(AcquireResp {
        task_id: req.task_id,
        devices: leases,
    }))
}

#[derive(Deserialize)]
struct ReleaseReq {
    task_id: String,
}

async fn release_devices(
    State(st): State<ControlApiState>,
    Json(req): Json<ReleaseReq>,
) -> Json<Value> {
    let mut map = st.leases.lock().unwrap();
    let before = map.len();
    map.retain(|_, l| l.task_id != req.task_id);
    Json(json!({ "released": before - map.len() }))
}

#[derive(Deserialize)]
struct InstallReq {
    apk_path: String,
    /// Install onto the devices leased to this task…
    #[serde(default)]
    task_id: Option<String>,
    /// …or onto these explicit serials (takes precedence over task_id).
    #[serde(default)]
    serials: Vec<String>,
}

async fn install(
    State(st): State<ControlApiState>,
    Json(req): Json<InstallReq>,
) -> Result<Json<Vec<CommandResult>>, (StatusCode, String)> {
    if !std::path::Path::new(&req.apk_path).is_file() {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("APK not found on this host: {}", req.apk_path),
        ));
    }

    let targets: Vec<DeviceRef> = if !req.serials.is_empty() {
        online_devices(&st.servers)
            .await
            .into_iter()
            .filter(|d| req.serials.contains(&d.serial))
            .collect()
    } else if let Some(task_id) = &req.task_id {
        st.leases
            .lock()
            .unwrap()
            .values()
            .filter(|l| &l.task_id == task_id)
            .map(|l| DeviceRef {
                serial: l.serial.clone(),
                server_host: l.server_host.clone(),
                server_port: l.server_port,
            })
            .collect()
    } else {
        online_devices(&st.servers).await
    };

    if targets.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "no target devices resolved".into()));
    }

    Ok(Json(
        install_apk(targets, req.apk_path, Arc::clone(&st.adb_semaphore)).await,
    ))
}

// ── Phase 2 stubs (not implemented yet) ───────────────────────────────────────

fn not_implemented(what: &str) -> (StatusCode, Json<Value>) {
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(json!({ "error": format!("{what} — Phase 2, not implemented yet") })),
    )
}

async fn capture_start() -> (StatusCode, Json<Value>) {
    not_implemented("capture/start (adb screenrecord + logcat to output-dir)")
}

async fn capture_stop() -> (StatusCode, Json<Value>) {
    not_implemented("capture/stop (finalize mp4/log artifacts)")
}

async fn report(Query(_q): Query<HashMap<String, String>>) -> (StatusCode, Json<Value>) {
    not_implemented("smoke/report (JUnit XML + JSON artifact bundle)")
}
