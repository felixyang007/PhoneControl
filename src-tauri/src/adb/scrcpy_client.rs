use std::{
    io::{Read, Write},
    net::TcpStream,
    sync::{Mutex, OnceLock},
    time::Duration,
};

use super::device::server_args;
use super::stream::StreamOptions;

/// Build the argv that starts the scrcpy server on the device.
/// Returns the tokens that should follow `adb -s SERIAL shell` — i.e. we
/// launch exactly like the official scrcpy CLI does:
///   `adb shell CLASSPATH=... app_process / com.genymobile.scrcpy.Server VER k=v k=v ...`
/// without an intermediate `sh -c` wrapper. Wrapping in `sh -c` has been
/// observed to suppress server stderr on some devices.
///
/// `tunnel_forward=true` tells the server to *listen* on the localabstract
/// socket (forward tunnel) instead of actively connecting back to the client
/// (reverse tunnel). Without this, the default `tunnel_forward=false` causes
/// the server to exit early when it cannot reach the client — which always
/// happens with remote ADB servers — leaving us with a forward-mapped port
/// that refuses connections.
pub(crate) fn build_start_argv(
    remote_path: &str,
    ver: &str,
    scid: u32,
    opts: &StreamOptions,
) -> Vec<String> {
    vec![
        format!("CLASSPATH={remote_path}"),
        "app_process".into(),
        "/".into(),
        "com.genymobile.scrcpy.Server".into(),
        ver.into(),
        format!("scid={scid:08x}"),
        "log_level=info".into(),
        "audio=false".into(),
        "control=false".into(),
        "tunnel_forward=true".into(),
        "stay_awake=true".into(),
        format!("max_size={}", opts.max_size),
        format!("max_fps={}", opts.max_fps),
        format!("video_bit_rate={}", opts.bit_rate),
        "video_codec_options=i-frame-interval=1".into(),
        "send_device_meta=false".into(),
        // Renamed in scrcpy 4.0. Passing the wrong name is not an error — the
        // server just logs "Unknown server option" and falls back to the
        // default — so this has to be right rather than merely accepted.
        if major_version_of(ver) >= 4 {
            "send_stream_meta=true".into()
        } else {
            "send_codec_meta=true".into()
        },
        "send_dummy_byte=true".into(),
        "send_frame_meta=true".into(),
    ]
}

/// Single-string form, kept only for tests. Production code uses
/// `build_start_argv` directly.
#[cfg(test)]
pub(crate) fn build_start_cmd(
    remote_path: &str,
    ver: &str,
    scid: u32,
    opts: &StreamOptions,
) -> String {
    build_start_argv(remote_path, ver, scid, opts).join(" ")
}

/// Local TCP port for the `adb forward` used to reach the scrcpy server.
///
/// Range `32200..=39999` gives 7800 ports. Birthday-problem collision
/// probability at 100 devices ≈ 0.6%, vs ~50% with the old 800-port range.
#[cfg(test)]
pub(crate) fn scrcpy_local_port(serial: &str) -> u16 {
    32200 + (fxhash::hash64(serial) % 7800) as u16
}

/// Address to connect to after `adb forward tcp:PORT ...` succeeds.
///
/// `adb forward` binds the listening socket on the ADB **server's** host, not
/// on the client machine. So for a local ADB server (`127.0.0.1` / `localhost`)
/// we connect to `127.0.0.1:PORT`; for a remote ADB server we must connect to
/// `{remote_host}:PORT` over the network. Hardcoding `127.0.0.1` here was the
/// bug that surfaced as "tcp connect failed ... Connection refused" for
/// every device on a remote ADB setup.
pub(crate) fn forward_connect_addr(server_host: &str, local_port: u16) -> String {
    format!("{server_host}:{local_port}")
}

/// Remote path for *our* copy of `scrcpy-server`.
///
/// Deliberately NOT `/data/local/tmp/scrcpy-server.jar`. The standalone scrcpy
/// client pushes to that exact path and **deletes it when it exits**. Sharing
/// the path means any scrcpy run — including this app's own "open in scrcpy"
/// button — wipes the jar out from under the embedded preview. Every later
/// `app_process` launch then aborts instantly with a missing CLASSPATH, and the
/// device sits on "reconnecting" with a black canvas until the app is
/// restarted. Owning a private filename makes the two independent.
pub(crate) const REMOTE_SERVER_PATH: &str = "/data/local/tmp/phone-control-scrcpy-server.jar";

fn scrcpy_version() -> Result<String, String> {
    let out = std::process::Command::new(super::binaries::scrcpy())
        .arg("--version")
        .output()
        .map_err(|e| format!("failed to run scrcpy --version: {e}"))?;
    let s = String::from_utf8_lossy(&out.stdout);
    // Output like: "scrcpy 3.2 <...>"
    let ver = s
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| "failed to parse scrcpy version".to_string())?;
    Ok(ver.to_string())
}

fn scrcpy_server_installed_path() -> Result<String, String> {
    // scrcpy provides the server path via SCRCPY_SERVER_PATH env when built from source.
    // For standard installs, we can ask scrcpy to print logs at debug level and rely on
    // the known default locations.
    // Here we support Homebrew default plus SCRCPY_SERVER_PATH override.
    if let Ok(p) = std::env::var("SCRCPY_SERVER_PATH") {
        return Ok(p);
    }

    let mut candidates: Vec<std::path::PathBuf> = Vec::new();

    // Derive from wherever the scrcpy binary actually lives: every packager
    // installs the server at `<prefix>/share/scrcpy/scrcpy-server` alongside
    // `<prefix>/bin/scrcpy`. This covers Homebrew on either architecture, and
    // any other prefix, without hardcoding it. `parent().parent()` is None for
    // a bare "scrcpy" fallback, so this is skipped when lookup failed.
    let bin = super::binaries::scrcpy();
    for base in [Some(bin.clone()), std::fs::canonicalize(&bin).ok()]
        .into_iter()
        .flatten()
    {
        if let Some(prefix) = base.parent().and_then(|p| p.parent()) {
            candidates.push(prefix.join("share/scrcpy/scrcpy-server"));
        }
    }

    // Homebrew's versioned `opt` symlinks, kept as a fallback.
    candidates.push("/opt/homebrew/opt/scrcpy/share/scrcpy/scrcpy-server".into());
    candidates.push("/usr/local/opt/scrcpy/share/scrcpy/scrcpy-server".into());
    candidates.push("/usr/share/scrcpy/scrcpy-server".into());
    candidates.push("/usr/local/share/scrcpy/scrcpy-server".into());

    for p in &candidates {
        if p.is_file() {
            return Ok(p.to_string_lossy().into_owned());
        }
    }

    Err("scrcpy-server path not found (set SCRCPY_SERVER_PATH)".into())
}

#[derive(Clone)]
struct ScrcpyRuntimeInfo {
    version: String,
    server_path: String,
    server_size: u64,
}

/// Caches only a *successful* probe.
///
/// This used to be a `OnceLock<Result<_, String>>`, which cached the `Err` too:
/// if scrcpy was missing when the first device connected, every later attempt
/// replayed that stale failure for the lifetime of the process, so installing
/// scrcpy required restarting the app and the UI just span on
/// "Stream: reconnecting" forever. Holding the lock across the probe is
/// intentional — it keeps N devices connecting at once from each spawning their
/// own `scrcpy --version`.
static SCRCPY_RUNTIME_INFO: OnceLock<Mutex<Option<ScrcpyRuntimeInfo>>> = OnceLock::new();

fn probe_scrcpy_runtime() -> Result<ScrcpyRuntimeInfo, String> {
    let version = scrcpy_version()?;
    let server_path = scrcpy_server_installed_path()?;
    let server_size = std::fs::metadata(&server_path)
        .map_err(|e| format!("scrcpy-server metadata failed: {e}"))?
        .len();
    if server_size == 0 {
        return Err("scrcpy-server file is empty".into());
    }
    Ok(ScrcpyRuntimeInfo {
        version,
        server_path,
        server_size,
    })
}

fn scrcpy_runtime_info() -> Result<ScrcpyRuntimeInfo, String> {
    let cell = SCRCPY_RUNTIME_INFO.get_or_init(|| Mutex::new(None));
    let mut cached = match cell.lock() {
        Ok(guard) => guard,
        // A panic while probing must not wedge streaming permanently.
        Err(poisoned) => poisoned.into_inner(),
    };
    if let Some(info) = cached.as_ref() {
        return Ok(info.clone());
    }

    let info = probe_scrcpy_runtime()?;
    println!(
        "[SCRCPY] runtime cached ver={} server={} size={}",
        info.version, info.server_path, info.server_size
    );
    *cached = Some(info.clone());
    Ok(info)
}

/// Layout of the scrcpy video socket, which changed incompatibly in 4.0.
///
/// Decoded from `Streamer`/`SurfaceEncoder` in the scrcpy 4.1 server dex, since
/// the wire format is not documented anywhere:
///
/// ```text
/// 3.x   [dummy 1] [codec_id 4 | width 4 | height 4]  then frames only
/// 4.x   [dummy 1] [codec_id 4]                       then frames + session meta
/// ```
///
/// In 4.x the video size is no longer a one-off header. `writeVideoHeader()`
/// emits just the codec id, and `writeSessionMeta(w, h, flag)` emits a 12-byte
/// record — `0x8000_000{0,1} | width | height`, no payload — before every
/// encoding session, i.e. again on each rotation or size change. Bit 63 of the
/// record's first 8 bytes is what distinguishes it from a frame header, which
/// is why the frame flags had to move down a bit:
///
/// ```text
/// 3.x   bit 63 = config,        bit 62 = keyframe,  pts = low 62 bits
/// 4.x   bit 63 = session meta,  bit 62 = config,    bit 61 = keyframe,
///                                                   pts = low 61 bits
/// ```
///
/// Reading a 4.x stream with 3.x flags is silently wrong rather than a clean
/// error: config packets (pts word `0x4000…`) look like keyframes and real
/// keyframes (`0x2000…`) look like deltas. The browser then configures its
/// decoder from a config packet mislabelled as a key chunk and waits forever
/// for a keyframe that never arrives — the stream reports "receiving" while the
/// canvas stays black.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoWireFormat {
    /// Bytes of one-off header following the dummy byte. Always starts with the
    /// 4-byte codec id; on 3.x it also carries width and height.
    pub header_len: usize,
    /// Whether width/height arrive as inline session-meta records instead.
    pub inline_session_meta: bool,
    pub config_mask: u64,
    pub key_mask: u64,
    pub pts_mask: u64,
}

/// Length of a session-meta record: `flags(4) + width(4) + height(4)`. Sized to
/// match a frame header so a reader can classify one before consuming it.
pub const SESSION_META_LEN: usize = 12;

/// Marks a session-meta record; also the frame header length, since scrcpy
/// deliberately made the two the same size.
pub const FRAME_HEADER_LEN: usize = 12;

pub fn video_wire_format(major_version: u32) -> VideoWireFormat {
    if major_version >= 4 {
        VideoWireFormat {
            header_len: 4,
            inline_session_meta: true,
            config_mask: 1 << 62,
            key_mask: 1 << 61,
            pts_mask: (1 << 61) - 1,
        }
    } else {
        VideoWireFormat {
            header_len: 12,
            inline_session_meta: false,
            config_mask: 1 << 63,
            key_mask: 1 << 62,
            pts_mask: (1 << 62) - 1,
        }
    }
}

/// Major version from a scrcpy version string like `4.1` or `3.2.1`.
/// Falls back to 3 (the historical layout) if it cannot be determined.
pub fn major_version_of(ver: &str) -> u32 {
    ver.split('.')
        .next()
        .and_then(|m| m.trim().parse::<u32>().ok())
        .unwrap_or(3)
}

/// Major version of the local scrcpy, used to pick the wire layout.
pub fn scrcpy_major_version() -> u32 {
    scrcpy_runtime_info()
        .ok()
        .map(|info| major_version_of(&info.version))
        .unwrap_or(3)
}

fn parse_first_u64(s: &str) -> Option<u64> {
    s.split(|c: char| !c.is_ascii_digit())
        .find(|part| !part.is_empty())
        .and_then(|part| part.parse().ok())
}

/// Minimal scrcpy bootstrapper.
///
/// Minimal scrcpy bootstrapper for embedded video preview.
pub struct ScrcpyConnection {
    pub serial: String,
    pub local_port: u16,
    pub stream: TcpStream,
    pub control: Option<TcpStream>,
    pub server_child: std::process::Child,
    pub scid: u32,
}

fn run_adb(host: &str, port: u16, args: &[String]) -> Result<std::process::Output, String> {
    let mut full = server_args(host, port);
    full.extend_from_slice(args);
    std::process::Command::new(super::binaries::adb())
        .args(&full)
        .output()
        .map_err(|e| format!("adb spawn failed: {e}"))
}

pub fn remove_forward(host: &str, port: u16, serial: &str, local_port: u16) {
    let _ = run_adb(
        host,
        port,
        &[
            "-s".into(),
            serial.into(),
            "forward".into(),
            "--remove".into(),
            format!("tcp:{local_port}").into(),
        ],
    );
}

fn run_adb_spawn(host: &str, port: u16, args: &[String]) -> Result<std::process::Child, String> {
    let mut full = server_args(host, port);
    full.extend_from_slice(args);
    std::process::Command::new(super::binaries::adb())
        .args(&full)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("adb spawn failed: {e}"))
}

pub fn terminate_child(child: &mut std::process::Child) {
    if matches!(child.try_wait(), Ok(Some(_))) {
        return;
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn spawn_log_pump(serial: &str, mut reader: impl Read + Send + 'static, stream_name: &'static str) {
    let serial = serial.to_string();
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        let mut pending = Vec::<u8>::new();
        loop {
            let n = match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(_) => break,
            };
            pending.extend_from_slice(&buf[..n]);
            while let Some(pos) = pending.iter().position(|&b| b == b'\n') {
                let line = pending.drain(..=pos).collect::<Vec<u8>>();
                let s = String::from_utf8_lossy(&line);
                print!("[SCRCPY-SERVER][{}][{}] {}", serial, stream_name, s);
            }
            if pending.len() > 1024 * 1024 {
                pending.clear();
            }
        }
        // Flush any trailing bytes that were not newline-terminated.
        // scrcpy crash output occasionally lacks a trailing \n and was
        // getting dropped silently, making failures look like clean exits.
        if !pending.is_empty() {
            let s = String::from_utf8_lossy(&pending);
            println!(
                "[SCRCPY-SERVER][{}][{}] (unterminated) {}",
                serial, stream_name, s
            );
        }
    });
}

fn adb_shell_check(host: &str, port: u16, serial: &str, cmd: &str) -> Result<String, String> {
    let out = run_adb(
        host,
        port,
        &[
            "-s".into(),
            serial.into(),
            "shell".into(),
            "sh".into(),
            "-c".into(),
            cmd.into(),
        ],
    )?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).to_string());
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

pub fn start_scrcpy_and_connect(
    serial: &str,
    server_host: &str,
    server_port: u16,
    opts: &StreamOptions,
) -> Result<ScrcpyConnection, String> {
    let started_at = std::time::Instant::now();
    // 0) Determine local scrcpy metadata once per app process. Running
    // `scrcpy --version` and probing the server path for every phone adds a
    // serialized cost to large pages.
    let runtime = scrcpy_runtime_info()?;
    let ver = runtime.version;

    // 1) Push server to device only if the copy there is missing or stale.
    let remote_path = REMOTE_SERVER_PATH;

    // The size probe below runs on EVERY start, deliberately. It used to be
    // skipped once a device was in `VERIFIED_REMOTE_SERVERS`, but "we pushed it
    // once" is not the same as "it is still there": `/data/local/tmp` is
    // cleared by a device reboot, by a wipe, and — observed on an Android 17
    // emulator — by the platform itself roughly 45s after the push. Once the
    // jar was gone, the cache meant we never looked again, so every reconnect
    // launched `app_process` against a missing CLASSPATH, aborted instantly,
    // and the device sat on "reconnecting" forever. One `stat` per start is
    // ~30ms; the expensive part was always the push, which is still skipped
    // when the sizes already match.
    {
        let remote_size_str = adb_shell_check(
            server_host,
            server_port,
            serial,
            &format!(
                "stat -c %s {0} 2>/dev/null || wc -c < {0} 2>/dev/null || echo 0",
                remote_path
            ),
        )
        .unwrap_or_else(|_| "0".to_string());
        let remote_size = parse_first_u64(&remote_size_str).unwrap_or(0);
        println!(
            "[SCRCPY] server size check serial={} local={} remote={} elapsed={}ms",
            serial,
            runtime.server_size,
            remote_size,
            started_at.elapsed().as_millis()
        );

        if runtime.server_size != remote_size {
            let out = run_adb(
                server_host,
                server_port,
                &[
                    "-s".into(),
                    serial.into(),
                    "push".into(),
                    runtime.server_path,
                    remote_path.into(),
                ],
            )?;
            if !out.status.success() {
                return Err(format!(
                    "adb push failed: {}",
                    String::from_utf8_lossy(&out.stderr)
                ));
            }
            println!(
                "[SCRCPY] server pushed serial={} remote={} ver={} elapsed={}ms",
                serial,
                remote_path,
                ver,
                started_at.elapsed().as_millis()
            );
        } else {
            println!(
                "[SCRCPY] server already on device serial={} ver={} elapsed={}ms",
                serial,
                ver,
                started_at.elapsed().as_millis()
            );
        }
    }

    // 2) Start scrcpy server on device.
    // scid identifies concurrent clients. Use random to avoid collision
    // with a previous server process that hasn't exited yet on the device.
    let scid = {
        use std::collections::hash_map::RandomState;
        use std::hash::{BuildHasher, Hasher};
        let mut h = RandomState::new().build_hasher();
        h.write(serial.as_bytes());
        (h.finish() as u32) & 0x7FFF_FFFF
    };
    // Note: args are key=value pairs, order irrelevant.
    let start_argv = build_start_argv(remote_path, &ver, scid, opts);

    // IMPORTANT: Keep the server process alive.
    // Do NOT start it in the background with '&' then let adb exit immediately,
    // otherwise the server is killed when the shell session ends on many devices.
    //
    // Launch style matches the official scrcpy CLI: `adb shell CLASSPATH=... app_process / ...`
    // with individual argv tokens. A `sh -c "..."` wrapper was observed to
    // silence server stderr on some devices (PKG110 / OPPO).
    let mut argv: Vec<String> = vec!["-s".into(), serial.into(), "shell".into()];
    argv.extend(start_argv);
    let mut server_child = run_adb_spawn(server_host, server_port, &argv)?;

    println!(
        "[SCRCPY] server started serial={} scid={:08x} elapsed={}ms (adb shell kept alive)",
        serial,
        scid,
        started_at.elapsed().as_millis()
    );

    if let Some(stdout) = server_child.stdout.take() {
        spawn_log_pump(serial, stdout, "stdout");
    }
    if let Some(stderr) = server_child.stderr.take() {
        spawn_log_pump(serial, stderr, "stderr");
    }

    // 3) Set up the forward tunnel.
    //
    // We always use forward tunnel (server listens on the localabstract socket,
    // client connects via `adb forward`). This is the only mode that works for
    // remote ADB servers, and it also works fine for local ADB. Using a single
    // mode avoids the reverse/forward state machine and the associated races.
    // The server was started with `tunnel_forward=true` to match.
    let socket_name = format!("localabstract:scrcpy_{:08x}", scid);

    // Use tcp:0 to let ADB pick a free port, avoiding "Address already in use"
    let out = run_adb(
        server_host,
        server_port,
        &[
            "-s".into(),
            serial.into(),
            "forward".into(),
            "tcp:0".into(),
            socket_name.clone().into(),
        ],
    )?;
    if !out.status.success() {
        terminate_child(&mut server_child);
        return Err(format!(
            "adb forward failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    // adb forward tcp:0 prints the allocated port on stdout
    let actual_port: u16 = String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .map_err(|e| format!("failed to parse allocated port: {e}"))?;
    let addr = forward_connect_addr(server_host, actual_port);

    println!(
        "[SCRCPY] tunnel forward serial={} port={} scid={:08x} adb={}:{} elapsed={}ms",
        serial,
        actual_port,
        scid,
        server_host,
        server_port,
        started_at.elapsed().as_millis()
    );

    // 4) Establish TCP connection with retry.
    //
    // The server may still be warming up (remote ADB can take a few hundred ms),
    // so we retry for up to 8 seconds. With `adb forward`, the TCP connect can
    // succeed immediately but the server hasn't bound the abstract socket yet,
    // causing an instant EOF. We detect this by peeking 1 byte — if it returns
    // 0 (EOF) we reconnect.
    let stream = {
        let mut last_err: Option<String>;
        let start = std::time::Instant::now();
        loop {
            match TcpStream::connect(&addr) {
                Ok(s) => {
                    s.set_read_timeout(Some(Duration::from_millis(300))).ok();
                    let mut peek = [0u8; 1];
                    match s.peek(&mut peek) {
                        Ok(0) => {
                            // Immediate EOF — server not ready yet
                            last_err = Some("immediate EOF (server not ready)".into());
                        }
                        Ok(_) => break s,
                        Err(e)
                            if e.kind() == std::io::ErrorKind::WouldBlock
                                || e.kind() == std::io::ErrorKind::TimedOut =>
                        {
                            // Timeout on peek means the connection is alive but no data yet — good
                            break s;
                        }
                        Err(e) => {
                            last_err = Some(format!("peek failed: {e}"));
                        }
                    }
                }
                Err(e) => {
                    last_err = Some(e.to_string());
                }
            }
            if start.elapsed() > Duration::from_secs(3) {
                terminate_child(&mut server_child);
                remove_forward(server_host, server_port, serial, actual_port);
                return Err(format!(
                    "tcp connect failed after retries: {}",
                    last_err.unwrap_or_else(|| "unknown".into())
                ));
            }
            std::thread::sleep(Duration::from_millis(40));
        }
    };

    println!(
        "[SCRCPY] tcp connected serial={} port={} elapsed={}ms",
        serial,
        actual_port,
        started_at.elapsed().as_millis()
    );

    stream.set_nodelay(true).ok();
    stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .ok();

    // Embedded multi-device preview prioritizes video startup. scrcpy control
    // requires a second socket accept; if that socket races or fails, some
    // devices sit forever after the dummy byte without producing frames.
    let control = None;

    // Keep the forward mapping for the whole scrcpy session, like the native
    // scrcpy client does. Removing it immediately leaves active connections
    // relying only on existing asocket pairs; with a remote ADB server and many
    // devices, batch control writes have been observed to make those sessions
    // EOF before the injected event is visible.
    println!(
        "[SCRCPY] forward listener kept serial={} port={} for active session",
        serial, actual_port
    );

    Ok(ScrcpyConnection {
        serial: serial.to_string(),
        local_port: actual_port,
        stream,
        control,
        server_child,
        scid,
    })
}

impl ScrcpyConnection {
    pub fn read_some(&mut self, max: usize) -> Result<Vec<u8>, String> {
        let mut buf = vec![0u8; max];
        let n = self
            .stream
            .read(&mut buf)
            .map_err(|e| format!("tcp read failed: {e}"))?;
        buf.truncate(n);
        Ok(buf)
    }

    pub fn write_all(&mut self, data: &[u8]) -> Result<(), String> {
        self.stream
            .write_all(data)
            .map_err(|e| format!("tcp write failed: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_start_cmd_includes_options() {
        let opts = StreamOptions {
            max_size: 1080,
            max_fps: 60,
            bit_rate: 8_000_000,
        };
        let cmd = build_start_cmd(REMOTE_SERVER_PATH, "3.2", 0xabcd, &opts);
        assert!(cmd.contains("max_size=1080"), "cmd={cmd}");
        assert!(cmd.contains("max_fps=60"), "cmd={cmd}");
        assert!(cmd.contains("video_bit_rate=8000000"), "cmd={cmd}");
        assert!(cmd.contains("scid=0000abcd"), "cmd={cmd}");
        assert!(
            cmd.contains(&format!("CLASSPATH={REMOTE_SERVER_PATH}")),
            "cmd={cmd}"
        );
        assert!(
            cmd.contains("com.genymobile.scrcpy.Server 3.2"),
            "cmd={cmd}"
        );
    }

    #[test]
    fn build_start_cmd_defaults() {
        let opts = StreamOptions::default();
        let cmd = build_start_cmd("/x.jar", "3.2", 1, &opts);
        assert!(cmd.contains("max_size=720"));
        assert!(cmd.contains("max_fps=30"));
        assert!(cmd.contains("video_bit_rate=4000000"));
        assert!(cmd.contains("audio=false"));
        assert!(cmd.contains("control=false"));
        assert!(cmd.contains("send_frame_meta=true"));
    }

    #[test]
    fn build_start_cmd_scid_padded_hex() {
        let opts = StreamOptions::default();
        // scid 0x1 should render as 8-char zero-padded hex
        let cmd = build_start_cmd("/x.jar", "3.2", 1, &opts);
        assert!(cmd.contains("scid=00000001"), "cmd={cmd}");
    }

    #[test]
    fn build_start_cmd_has_tunnel_forward() {
        // Missing tunnel_forward=true makes the server default to reverse
        // tunnel, which exits immediately for remote ADB. Regression guard.
        let cmd = build_start_cmd("/x.jar", "3.2", 1, &StreamOptions::default());
        assert!(cmd.contains("tunnel_forward=true"), "cmd={cmd}");
    }

    #[test]
    fn scrcpy_local_port_avoids_ws_server_port() {
        // WS server listens on 127.0.0.1:32199. Forward port must never land
        // on it, regardless of the serial.
        for serial in ["", "a", "3B65BQ01MW300000", "device-测试", "emulator-5554"] {
            let p = scrcpy_local_port(serial);
            assert_ne!(p, 32199, "serial={serial} -> port {p} collides with WS");
            assert!(
                (32200..=39999).contains(&p),
                "serial={serial} -> port {p} out of range"
            );
        }
    }

    #[test]
    fn scrcpy_local_port_is_deterministic() {
        // Start/cleanup paths compute the port independently; they must agree.
        let s = "3B65BQ01MW300000";
        assert_eq!(scrcpy_local_port(s), scrcpy_local_port(s));
    }

    #[test]
    fn forward_connect_addr_uses_server_host() {
        // For a remote ADB server, `adb forward` listens on the REMOTE host.
        // Connecting to 127.0.0.1 here was the "Connection refused" bug.
        assert_eq!(
            forward_connect_addr("192.168.0.136", 32278),
            "192.168.0.136:32278"
        );
        assert_eq!(forward_connect_addr("10.0.0.5", 32200), "10.0.0.5:32200");
    }

    #[test]
    fn forward_connect_addr_local_adb() {
        // Local ADB path still works: the server host IS 127.0.0.1.
        assert_eq!(forward_connect_addr("127.0.0.1", 32250), "127.0.0.1:32250");
        assert_eq!(forward_connect_addr("localhost", 32250), "localhost:32250");
    }

    /// Regression: scrcpy 4.0 shrank the video header to a bare codec id and
    /// moved the video size into inline session-meta records.
    #[test]
    fn video_header_len_tracks_scrcpy_major() {
        assert_eq!(video_wire_format(2).header_len, 12);
        assert_eq!(video_wire_format(3).header_len, 12);
        assert_eq!(video_wire_format(4).header_len, 4);
        assert_eq!(video_wire_format(5).header_len, 4);

        assert!(!video_wire_format(3).inline_session_meta);
        assert!(video_wire_format(4).inline_session_meta);
    }

    /// The bug behind the black canvas: scrcpy 4.x gave bit 63 to session meta
    /// and shifted the frame flags down one. Read with 3.x masks, a 4.x config
    /// packet looks like a keyframe and a 4.x keyframe looks like a delta — so
    /// the browser configures its decoder and then waits forever for a key
    /// chunk that never comes.
    #[test]
    fn frame_flag_bits_shifted_in_scrcpy_4x() {
        let v3 = video_wire_format(3);
        assert_eq!(v3.config_mask, 0x8000_0000_0000_0000);
        assert_eq!(v3.key_mask, 0x4000_0000_0000_0000);

        let v4 = video_wire_format(4);
        assert_eq!(v4.config_mask, 0x4000_0000_0000_0000);
        assert_eq!(v4.key_mask, 0x2000_0000_0000_0000);

        // Exactly what `Streamer.writeFrameMeta` emits on 4.x.
        let config_hdr: u64 = 0x4000_0000_0000_0000;
        let key_hdr: u64 = 0x2000_0000_0000_0000 | 123_456;
        let delta_hdr: u64 = 123_999;

        assert!(config_hdr & v4.config_mask != 0);
        assert!(key_hdr & v4.config_mask == 0 && key_hdr & v4.key_mask != 0);
        assert!(delta_hdr & v4.config_mask == 0 && delta_hdr & v4.key_mask == 0);
        assert_eq!(key_hdr & v4.pts_mask, 123_456);

        // Session meta must not be mistaken for any of them.
        assert!(0x8000_0000_u64 << 32 & v4.config_mask == 0);

        // The pre-fix misread, kept as documentation.
        assert!(
            config_hdr & v3.key_mask != 0,
            "config looked like a keyframe"
        );
        assert!(key_hdr & v3.key_mask == 0, "keyframe looked like a delta");
    }

    /// Byte-exact check against a real scrcpy 4.1 stream from a 1080x2400
    /// device at `max_size=480`: dummy byte, "h264", then a session-meta record
    /// carrying 216x480.
    #[test]
    fn scrcpy_4x_header_then_session_meta_match_capture() {
        let captured: [u8; 17] = [
            0x00, // dummy byte
            0x68, 0x32, 0x36, 0x34, // video header: "h264" — and nothing else
            0x80, 0x00, 0x00, 0x00, // session meta: 0x80000000 marker + flag
            0x00, 0x00, 0x00, 0xd8, // width  = 216
            0x00, 0x00, 0x01, 0xe0, // height = 480
        ];
        let wire = video_wire_format(4);
        let after_dummy = &captured[1..];
        let meta = &after_dummy[wire.header_len..];
        assert_eq!(meta.len(), SESSION_META_LEN);

        // A reader sees the first 8 bytes as a frame header's pts word first.
        let as_frame_header = u64::from_be_bytes(meta[0..8].try_into().unwrap());
        assert!(
            as_frame_header & (1 << 63) != 0,
            "must be tagged as session meta"
        );
        assert_eq!(u32::from_be_bytes(meta[4..8].try_into().unwrap()), 216);
        assert_eq!(u32::from_be_bytes(meta[8..12].try_into().unwrap()), 480);
    }

    /// Regression: the embedded preview went permanently black whenever a
    /// standalone scrcpy ran, because scrcpy deletes its own pushed server on
    /// exit and we were pushing to the same filename.
    #[test]
    fn remote_server_path_does_not_collide_with_scrcpy() {
        assert_ne!(
            REMOTE_SERVER_PATH, "/data/local/tmp/scrcpy-server.jar",
            "standalone scrcpy deletes this path on exit; we must own a private one"
        );
        assert!(REMOTE_SERVER_PATH.starts_with("/data/local/tmp/"));
    }

    #[test]
    fn build_start_argv_is_individual_tokens() {
        // scrcpy CLI uses individual argv tokens — NOT `sh -c "..."`. The
        // sh -c wrapper was observed to swallow server stderr on OPPO PKG110.
        // Each argument must stand alone so `adb shell` gets them as separate
        // args, the way the CLI does.
        let argv = build_start_argv(REMOTE_SERVER_PATH, "3.2", 0xabcd, &StreamOptions::default());

        // Must NOT contain spaces in any single token (would indicate a joined blob).
        // Note: some scrcpy key=value args legitimately contain multiple '=' signs
        // (e.g. "video_codec_options=i-frame-interval=1"), so we only check for spaces.
        for token in &argv {
            assert!(
                !token.contains(' '),
                "token {token:?} contains a space — still a joined blob"
            );
        }

        // Required tokens, in argv form.
        assert_eq!(argv[0], format!("CLASSPATH={REMOTE_SERVER_PATH}"));
        assert_eq!(argv[1], "app_process");
        assert_eq!(argv[2], "/");
        assert_eq!(argv[3], "com.genymobile.scrcpy.Server");
        assert_eq!(argv[4], "3.2");
        assert!(argv.iter().any(|a| a == "scid=0000abcd"));
        assert!(argv.iter().any(|a| a == "tunnel_forward=true"));
        assert!(argv.iter().any(|a| a == "audio=false"));
        assert!(argv.iter().any(|a| a == "control=false"));
        assert!(argv.iter().any(|a| a == "stay_awake=true"));
    }

    #[test]
    fn build_start_argv_threads_stream_options() {
        let opts = StreamOptions {
            max_size: 1080,
            max_fps: 60,
            bit_rate: 8_000_000,
        };
        let argv = build_start_argv("/x.jar", "3.2", 1, &opts);
        assert!(argv.iter().any(|a| a == "max_size=1080"));
        assert!(argv.iter().any(|a| a == "max_fps=60"));
        assert!(argv.iter().any(|a| a == "video_bit_rate=8000000"));
    }
}
