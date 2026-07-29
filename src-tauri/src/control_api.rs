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

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::process::Command as TokioCommand;

use axum::{
    extract::{Query, Request, State},
    http::{header::AUTHORIZATION, StatusCode},
    middleware::{self, Next},
    response::Response,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::AppHandle;
use tokio::sync::{Mutex, Semaphore};

use crate::adb::binaries::adb as adb_bin;
use crate::adb::stream::{start_stream_loop, StreamOptions};
use crate::recording::Recorder;

/// Default lease lifetime (15 min). A crashed/killed CI job that never calls
/// `/release` has its lease reclaimed by the background sweeper after this.
const DEFAULT_TTL_SECS: u64 = 15 * 60;
/// How often the sweeper scans for expired leases.
const SWEEP_INTERVAL_SECS: u64 = 30;

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn token_file() -> PathBuf {
    let mut p = dirs_next::home_dir().unwrap_or_else(|| PathBuf::from("."));
    p.push(".phone_control");
    let _ = std::fs::create_dir_all(&p);
    p.push("api_token");
    p
}

/// Resolve the API bearer token: `PHONE_CONTROL_TOKEN` env wins; otherwise read
/// (or lazily generate) `~/.phone_control/api_token`. CI reads the same file.
pub fn resolve_api_token() -> String {
    if let Ok(tok) = std::env::var("PHONE_CONTROL_TOKEN") {
        if !tok.trim().is_empty() {
            return tok.trim().to_string();
        }
    }
    let path = token_file();
    if let Ok(tok) = std::fs::read_to_string(&path) {
        if !tok.trim().is_empty() {
            return tok.trim().to_string();
        }
    }
    let tok = uuid::Uuid::new_v4().to_string();
    let _ = std::fs::write(&path, &tok);
    tok
}

/// Path shown in logs so a human/CI can find the token (never log the token).
pub fn token_file_display() -> String {
    token_file().display().to_string()
}

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
    /// Epoch seconds; reclaimed by the sweeper once passed (TTL safety net).
    pub expires_at: u64,
}

/// One device's artifacts from a smoke run (what `/smoke/report` returns).
#[derive(Debug, Clone, Serialize)]
pub struct DeviceArtifact {
    pub serial: String,
    pub mp4: Option<String>,
    pub logcat: Option<String>,
    pub frames: u64,
}

/// An in-flight capture's logcat side (the mp4 side lives in `Recorders`).
struct ActiveCapture {
    logcat_path: String,
    logcat: Option<tokio::process::Child>,
}

/// Shared handles the API borrows from `AppState`, plus its own registries.
#[derive(Clone)]
pub struct ControlApiState {
    pub servers: Arc<Mutex<Vec<AdbServer>>>,
    pub adb_semaphore: Arc<Semaphore>,
    pub stream_tokens: StreamTokens,
    pub control_sockets: ControlSockets,
    pub leases: Arc<StdMutex<HashMap<String, Lease>>>,
    /// Bearer token required on every endpoint except `/health`.
    pub token: String,
    /// Active recordings, keyed by serial (shared with the scrcpy receive loop).
    pub recorders: crate::recording::Recorders,
    /// In-flight logcat captures, keyed by serial.
    captures: Arc<StdMutex<HashMap<String, ActiveCapture>>>,
    /// Finished artifacts accumulated per task_id, for `/smoke/report`.
    reports: Arc<StdMutex<HashMap<String, Vec<DeviceArtifact>>>>,
    /// Needed to (re)start a video-only stream to feed the recorder.
    pub app: AppHandle,
}

impl ControlApiState {
    pub fn new(
        servers: Arc<Mutex<Vec<AdbServer>>>,
        adb_semaphore: Arc<Semaphore>,
        stream_tokens: StreamTokens,
        control_sockets: ControlSockets,
        token: String,
        recorders: crate::recording::Recorders,
        app: AppHandle,
    ) -> Self {
        Self {
            servers,
            adb_semaphore,
            stream_tokens,
            control_sockets,
            leases: Arc::new(StdMutex::new(HashMap::new())),
            token,
            recorders,
            captures: Arc::new(StdMutex::new(HashMap::new())),
            reports: Arc::new(StdMutex::new(HashMap::new())),
            app,
        }
    }
}

/// Bearer-token gate for every endpoint except `/health`. Even though the API
/// binds loopback only, this stops stray local processes from poking ADB.
async fn require_auth(
    State(st): State<ControlApiState>,
    req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let provided = req
        .headers()
        .get(AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .unwrap_or("")
        .trim();
    if !st.token.is_empty() && provided == st.token {
        Ok(next.run(req).await)
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}

/// Background sweeper: reclaim leases whose TTL has passed (crashed/killed CI
/// jobs that never called `/release`).
fn spawn_lease_sweeper(leases: Arc<StdMutex<HashMap<String, Lease>>>) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(SWEEP_INTERVAL_SECS)).await;
            let now = now_secs();
            let mut map = leases.lock().unwrap();
            let before = map.len();
            map.retain(|_, l| l.expires_at > now);
            let removed = before - map.len();
            if removed > 0 {
                println!("[CTRL-API] swept {removed} expired lease(s)");
            }
        }
    });
}

/// Start the control API. Spawned from `lib.rs::setup`, alongside the WS hub.
/// Binds loopback only — never expose this off-box.
pub async fn run_control_api(state: ControlApiState, addr: SocketAddr) -> Result<(), String> {
    spawn_lease_sweeper(Arc::clone(&state.leases));

    // Everything except /health sits behind the bearer-token gate.
    let protected = Router::new()
        .route("/api/v1/devices", get(list_devices))
        .route("/api/v1/devices/acquire", post(acquire_devices))
        .route("/api/v1/devices/release", post(release_devices))
        .route("/api/v1/install", post(install))
        .route("/api/v1/capture/start", post(capture_start))
        .route("/api/v1/capture/stop", post(capture_stop))
        .route("/api/v1/smoke/report", get(report))
        .route_layer(middleware::from_fn_with_state(state.clone(), require_auth));

    let app = Router::new()
        .route("/api/v1/health", get(health))
        .merge(protected)
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| format!("control-api bind {addr}: {e}"))?;
    println!("[CTRL-API] listening on http://{addr} (bearer-token protected)");
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
                "leased_until": leased.get(&d.serial).map(|l| l.expires_at),
            })
        })
        .collect();
    Json(json!({ "devices": list }))
}

#[derive(Deserialize)]
struct AcquireReq {
    task_id: String,
    /// Exact device to lease (compatibility repro). Absent = "any" idle device
    /// — the common case. One device per call; Jenkins parallel jobs each call
    /// `/acquire` independently (no multi-device group binding).
    #[serde(default)]
    serial: Option<String>,
    /// Lease lifetime in seconds; defaults to 15 min. Auto-reclaimed on expiry.
    #[serde(default)]
    ttl_secs: Option<u64>,
}

#[derive(Serialize)]
struct AcquireResp {
    task_id: String,
    device: Lease,
}

async fn acquire_devices(
    State(st): State<ControlApiState>,
    Json(req): Json<AcquireReq>,
) -> Result<Json<AcquireResp>, (StatusCode, String)> {
    let available = online_devices(&st.servers).await;
    let leased: std::collections::HashSet<String> =
        { st.leases.lock().unwrap().keys().cloned().collect() };

    let chosen: DeviceRef = match &req.serial {
        // `serial` filter — exact device.
        Some(serial) => {
            if leased.contains(serial) {
                return Err((StatusCode::CONFLICT, format!("device {serial} already leased")));
            }
            available
                .into_iter()
                .find(|d| &d.serial == serial)
                .ok_or((StatusCode::CONFLICT, format!("device {serial} not online")))?
        }
        // `any` filter — first idle device.
        None => available
            .into_iter()
            .find(|d| !leased.contains(&d.serial))
            .ok_or((StatusCode::CONFLICT, "no idle device available".to_string()))?,
    };

    // Decision #3 (refined): release only the *control* socket so phone-control
    // can't inject input while Maestro drives — but KEEP the video-only mirror
    // running so an operator can watch the automation live in the UI. A
    // read-only H.264 mirror doesn't contend with Maestro's UiAutomator input
    // channel (proven: recording streams video the whole time Maestro runs).
    st.control_sockets.lock().unwrap().remove(&chosen.serial);

    let ttl = req.ttl_secs.unwrap_or(DEFAULT_TTL_SECS);
    let lease = Lease {
        serial: chosen.serial.clone(),
        server_host: chosen.server_host,
        server_port: chosen.server_port,
        task_id: req.task_id.clone(),
        expires_at: now_secs() + ttl,
    };
    st.leases
        .lock()
        .unwrap()
        .insert(chosen.serial.clone(), lease.clone());

    Ok(Json(AcquireResp {
        task_id: req.task_id,
        device: lease,
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

// ── Capture (screen recording, decision #2) ───────────────────────────────────

/// Resolve host/port for a serial: prefer its lease, else look it up live.
async fn resolve_endpoint(st: &ControlApiState, serial: &str) -> Option<(String, u16)> {
    if let Some(l) = st.leases.lock().unwrap().get(serial) {
        return Some((l.server_host.clone(), l.server_port));
    }
    online_devices(&st.servers)
        .await
        .into_iter()
        .find(|d| d.serial == serial)
        .map(|d| (d.server_host, d.server_port))
}

fn recordings_dir() -> PathBuf {
    let mut p = dirs_next::home_dir().unwrap_or_else(|| PathBuf::from("."));
    p.push(".phone_control");
    p.push("recordings");
    let _ = std::fs::create_dir_all(&p);
    p
}

/// Synthetic stream client id for a recording, so start/stop pair up cleanly.
fn rec_client_id(serial: &str) -> String {
    format!("recorder:{serial}")
}

#[derive(Deserialize)]
struct CaptureStartReq {
    /// Record the device leased to this task…
    #[serde(default)]
    task_id: Option<String>,
    /// …or this explicit serial (takes precedence).
    #[serde(default)]
    serial: Option<String>,
    /// Directory for the mp4 (default `~/.phone_control/recordings`).
    #[serde(default)]
    output_dir: Option<String>,
}

async fn capture_start(
    State(st): State<ControlApiState>,
    Json(req): Json<CaptureStartReq>,
) -> Result<Json<Value>, (StatusCode, String)> {
    // Resolve the target serial: explicit, else the task's leased device.
    let serial = match req.serial {
        Some(s) => s,
        None => {
            let task = req
                .task_id
                .as_ref()
                .ok_or((StatusCode::BAD_REQUEST, "serial or task_id required".into()))?;
            st.leases
                .lock()
                .unwrap()
                .values()
                .find(|l| &l.task_id == task)
                .map(|l| l.serial.clone())
                .ok_or((StatusCode::BAD_REQUEST, format!("no device leased to {task}")))?
        }
    };

    let (host, port) = resolve_endpoint(&st, &serial)
        .await
        .ok_or((StatusCode::CONFLICT, format!("device {serial} not online")))?;

    let task_id = req.task_id.clone().unwrap_or_else(|| "adhoc".to_string());

    // Paired artifact paths: <dir>/<task>-<serial>-<epoch>.{mp4,logcat.txt}.
    let dir = req.output_dir.map(PathBuf::from).unwrap_or_else(recordings_dir);
    let _ = std::fs::create_dir_all(&dir);
    let safe_serial = serial.replace([':', '/'], "_");
    let stem = format!("{task_id}-{safe_serial}-{}", now_secs());
    let mp4_path = dir.join(format!("{stem}.mp4")).display().to_string();
    let logcat_path = dir.join(format!("{stem}.logcat.txt")).display().to_string();

    // Register the recorder. One per serial.
    {
        let mut map = st.recorders.lock().unwrap();
        if map.contains_key(&serial) {
            return Err((StatusCode::CONFLICT, format!("already recording {serial}")));
        }
        map.insert(serial.clone(), Recorder::new(mp4_path.clone(), task_id.clone()));
    }

    // Logcat capture (best-effort): clear the buffer so we only get this run's
    // window, then stream `adb logcat` to a file until capture/stop kills it.
    let prefix = server_args(&host, port);
    let mut clear = prefix.clone();
    clear.extend(["-s".into(), serial.clone(), "logcat".into(), "-c".into()]);
    let _ = TokioCommand::new(adb_bin()).args(&clear).status().await;

    let logcat_child = match std::fs::File::create(&logcat_path) {
        Ok(f) => {
            let mut args = prefix.clone();
            args.extend([
                "-s".into(),
                serial.clone(),
                "logcat".into(),
                "-v".into(),
                "threadtime".into(),
            ]);
            TokioCommand::new(adb_bin())
                .args(&args)
                .stdout(Stdio::from(f))
                .stderr(Stdio::null())
                .spawn()
                .map_err(|e| eprintln!("[CTRL-API] logcat spawn failed: {e}"))
                .ok()
        }
        Err(e) => {
            eprintln!("[CTRL-API] logcat file create failed: {e}");
            None
        }
    };
    st.captures.lock().unwrap().insert(
        serial.clone(),
        ActiveCapture {
            logcat_path: logcat_path.clone(),
            logcat: logcat_child,
        },
    );

    // (Re)start a video-only stream so frames flow to the recorder tap.
    tauri::async_runtime::spawn(start_stream_loop(
        Arc::clone(&st.stream_tokens),
        Arc::clone(&st.control_sockets),
        Arc::clone(&st.adb_semaphore),
        serial.clone(),
        host,
        port,
        StreamOptions::default(),
        rec_client_id(&serial),
        st.app.clone(),
    ));

    println!("[CTRL-API] capture start serial={serial} task={task_id} → {mp4_path}");
    Ok(Json(json!({
        "task_id": task_id,
        "serial": serial,
        "output": mp4_path,
        "logcat": logcat_path,
    })))
}

#[derive(Deserialize)]
struct CaptureStopReq {
    #[serde(default)]
    task_id: Option<String>,
    #[serde(default)]
    serial: Option<String>,
}

async fn capture_stop(
    State(st): State<ControlApiState>,
    Json(req): Json<CaptureStopReq>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let serial = match req.serial {
        Some(s) => s,
        None => {
            let task = req
                .task_id
                .as_ref()
                .ok_or((StatusCode::BAD_REQUEST, "serial or task_id required".into()))?;
            st.recorders
                .lock()
                .unwrap()
                .iter()
                .find(|(_, r)| &r.task_id == task)
                .map(|(s, _)| s.clone())
                .ok_or((StatusCode::NOT_FOUND, format!("no recording for task {task}")))?
        }
    };

    // Remove the recorder so the tap stops feeding it, then finalise the mp4.
    let recorder = st
        .recorders
        .lock()
        .unwrap()
        .remove(&serial)
        .ok_or((StatusCode::NOT_FOUND, format!("no recording for {serial}")))?;
    let task_id = recorder.task_id.clone();

    // Release the recorder's stream client (ref-counted, not forced): if an
    // operator is watching this device live, their stream stays up; if the
    // recorder was the only client, the stream stops.
    stop_stream_loop(
        Arc::clone(&st.stream_tokens),
        Arc::clone(&st.control_sockets),
        &serial,
        None,
        None,
        Some(&rec_client_id(&serial)),
        false,
    )
    .await;

    let (mp4, frames) = recorder
        .finish()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;

    // Stop logcat (remove from map first so we don't await while holding the lock).
    let capture = st.captures.lock().unwrap().remove(&serial);
    let logcat = match capture {
        Some(mut cap) => {
            if let Some(mut child) = cap.logcat.take() {
                let _ = child.kill().await;
            }
            Some(cap.logcat_path)
        }
        None => None,
    };

    // Accumulate the artifact under its task for later `/smoke/report`.
    let artifact = DeviceArtifact {
        serial: serial.clone(),
        mp4: Some(mp4.clone()),
        logcat: logcat.clone(),
        frames,
    };
    st.reports
        .lock()
        .unwrap()
        .entry(task_id.clone())
        .or_default()
        .push(artifact.clone());

    println!("[CTRL-API] capture stop serial={serial} task={task_id} frames={frames} → {mp4}");
    Ok(Json(json!({
        "serial": serial,
        "task_id": task_id,
        "output": mp4,
        "logcat": logcat,
        "frames": frames,
    })))
}

// ── Report (artifact bundle) ──────────────────────────────────────────────────

/// `GET /api/v1/smoke/report?task_id=…` — the artifacts phone-control produced
/// for a task (mp4 + logcat per device), plus a manifest.json written next to
/// them. The pass/fail JUnit itself comes from Maestro; this bundles what the
/// device side captured so CI can attach everything to one report/Linear bug.
async fn report(
    State(st): State<ControlApiState>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let task_id = q
        .get("task_id")
        .cloned()
        .ok_or((StatusCode::BAD_REQUEST, "task_id query param required".into()))?;

    let artifacts = st
        .reports
        .lock()
        .unwrap()
        .get(&task_id)
        .cloned()
        .ok_or((StatusCode::NOT_FOUND, format!("no artifacts for task {task_id}")))?;

    let manifest = json!({ "task_id": task_id, "artifacts": artifacts });

    // Write manifest.json next to the artifacts (dir of the first mp4).
    let mut manifest_path = None;
    if let Some(dir) = artifacts
        .iter()
        .find_map(|a| a.mp4.as_ref())
        .and_then(|p| PathBuf::from(p).parent().map(|d| d.to_path_buf()))
    {
        let path = dir.join(format!("{task_id}-manifest.json"));
        if let Ok(text) = serde_json::to_string_pretty(&manifest) {
            if std::fs::write(&path, text).is_ok() {
                manifest_path = Some(path.display().to_string());
            }
        }
    }

    Ok(Json(json!({
        "task_id": task_id,
        "artifacts": artifacts,
        "manifest": manifest_path,
    })))
}
