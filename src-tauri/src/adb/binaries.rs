//! Locating the external `adb` and `scrcpy` executables.
//!
//! Every call site used to spawn these by bare name (`Command::new("adb")`),
//! which relies entirely on the PATH this process happens to have inherited.
//! That holds under `npm run tauri dev` — the child inherits the shell's PATH,
//! Homebrew included — but *not* for a bundled `.app` launched from Finder or
//! the Dock. macOS hands a GUI app a minimal PATH of
//! `/usr/bin:/bin:/usr/sbin:/sbin`, so `/opt/homebrew/bin` is absent and the
//! spawn fails with "No such file or directory (os error 2)" even though the
//! user has both tools installed.
//!
//! Resolution order, per binary:
//!   1. an explicit override env var (`ADB_PATH` / `SCRCPY_PATH`)
//!   2. the inherited PATH
//!   3. well-known install locations for the platform
//!   4. the bare name, so a genuinely-missing tool still produces the familiar
//!      error text rather than a confusing absolute path
//!
//! A successful lookup is cached for the life of the process; a failed one is
//! deliberately *not* cached, so installing the tool while the app is running
//! is picked up on the next attempt instead of requiring a restart.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

fn is_executable(p: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(p)
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        p.is_file()
    }
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

/// Directories that hold user-installed CLI tools on this platform.
fn common_bin_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if cfg!(windows) {
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            dirs.push(PathBuf::from(local).join(r"Android\Sdk\platform-tools"));
        }
    } else {
        // Homebrew on Apple Silicon, Homebrew on Intel, then the system dirs.
        dirs.push(PathBuf::from("/opt/homebrew/bin"));
        dirs.push(PathBuf::from("/usr/local/bin"));
        dirs.push(PathBuf::from("/usr/bin"));
        dirs.push(PathBuf::from("/bin"));
        if let Some(home) = home_dir() {
            dirs.push(home.join(".local/bin"));
        }
    }
    dirs
}

/// `adb` additionally ships inside the Android SDK, which is where it lives for
/// anyone who installed Android Studio rather than `android-platform-tools`.
///
/// Order matters, and it is deliberately *not* SDK-first. A machine can easily
/// carry two different adb builds — Homebrew's and Android Studio's — and two
/// adb clients of differing versions repeatedly kill and restart each other's
/// server, which surfaces here as devices dropping mid-session. The dirs below
/// are searched only when PATH missed (the Finder-launch case), so they are
/// ordered to land on the *same* binary a terminal would pick: an explicitly
/// configured SDK first, then the PATH-typical locations, then SDK defaults
/// that are usually absent from PATH. `ADB_PATH` overrides all of it.
fn adb_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    for var in ["ANDROID_HOME", "ANDROID_SDK_ROOT"] {
        if let Some(root) = std::env::var_os(var) {
            dirs.push(PathBuf::from(root).join("platform-tools"));
        }
    }
    dirs.extend(common_bin_dirs());
    if let Some(home) = home_dir() {
        dirs.push(home.join("Library/Android/sdk/platform-tools"));
        dirs.push(home.join("Android/Sdk/platform-tools"));
    }
    dirs
}

fn scrcpy_dirs() -> Vec<PathBuf> {
    common_bin_dirs()
}

fn find_in_dirs(file: &str, dirs: impl IntoIterator<Item = PathBuf>) -> Option<PathBuf> {
    dirs.into_iter()
        .map(|dir| dir.join(file))
        .find(|candidate| is_executable(candidate))
}

fn path_dirs() -> Vec<PathBuf> {
    match std::env::var_os("PATH") {
        Some(path) => std::env::split_paths(&path).collect(),
        None => Vec::new(),
    }
}

/// Split out from [`resolve`] so tests can inject an override without calling
/// `std::env::set_var`, which races with every other thread reading the
/// environment in the same test binary.
fn resolve_with(
    name: &str,
    override_var: &str,
    override_value: Option<PathBuf>,
    dirs: Vec<PathBuf>,
) -> Option<PathBuf> {
    if let Some(overridden) = override_value {
        if is_executable(&overridden) {
            return Some(overridden);
        }
        eprintln!(
            "[BIN] {} is set to {} which is not an executable file; ignoring it",
            override_var,
            overridden.display()
        );
    }

    let file = format!("{name}{}", std::env::consts::EXE_SUFFIX);
    find_in_dirs(&file, path_dirs()).or_else(|| find_in_dirs(&file, dirs))
}

fn resolve(name: &str, override_var: &str, dirs: Vec<PathBuf>) -> Option<PathBuf> {
    let override_value = std::env::var_os(override_var).map(PathBuf::from);
    resolve_with(name, override_var, override_value, dirs)
}

fn resolved(
    cache: &'static OnceLock<PathBuf>,
    name: &'static str,
    override_var: &'static str,
    dirs: fn() -> Vec<PathBuf>,
) -> PathBuf {
    if let Some(hit) = cache.get() {
        return hit.clone();
    }
    match resolve(name, override_var, dirs()) {
        Some(found) => {
            println!("[BIN] resolved {} -> {}", name, found.display());
            let _ = cache.set(found.clone());
            found
        }
        None => PathBuf::from(name),
    }
}

static ADB: OnceLock<PathBuf> = OnceLock::new();
static SCRCPY: OnceLock<PathBuf> = OnceLock::new();

/// Absolute path to `adb`, or the bare name if it could not be found.
pub fn adb() -> PathBuf {
    resolved(&ADB, "adb", "ADB_PATH", adb_dirs)
}

/// Absolute path to `scrcpy`, or the bare name if it could not be found.
pub fn scrcpy() -> PathBuf {
    resolved(&SCRCPY, "scrcpy", "SCRCPY_PATH", scrcpy_dirs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_binary_falls_back_to_bare_name() {
        // Nothing named this exists, so the caller still gets a spawnable name
        // and the resulting error message stays recognisable.
        let got = resolve("phone-control-no-such-tool", "PC_NO_SUCH_VAR", vec![]);
        assert!(got.is_none());
    }

    #[test]
    fn path_lookup_finds_a_real_tool() {
        // `sh` is on PATH on every unix CI runner and dev machine.
        #[cfg(unix)]
        {
            let got = resolve("sh", "PC_UNSET_OVERRIDE_VAR", vec![]);
            assert!(got.is_some(), "expected to find sh on PATH");
            assert!(is_executable(&got.unwrap()));
        }
    }

    #[test]
    fn well_known_dir_is_scanned_when_path_misses() {
        // The dirs scan must work on its own, independent of PATH — that is the
        // whole point of the fallback for Finder-launched bundles.
        #[cfg(unix)]
        {
            assert_eq!(
                find_in_dirs("sh", vec![PathBuf::from("/bin")]),
                Some(PathBuf::from("/bin/sh"))
            );
            assert_eq!(
                find_in_dirs("phone-control-no-such-tool", vec![PathBuf::from("/bin")]),
                None
            );
            // A directory that does not exist must not abort the scan.
            assert_eq!(
                find_in_dirs(
                    "sh",
                    vec![PathBuf::from("/no/such/dir"), PathBuf::from("/bin")]
                ),
                Some(PathBuf::from("/bin/sh"))
            );
        }
    }

    /// The exact regression: a bundled `.app` resolves through the well-known
    /// dirs (no Homebrew on its PATH) while a terminal run resolves through
    /// PATH. If those two disagree, the GUI silently drives a different adb
    /// than the CLI — and mismatched adb versions fight over the adb server.
    /// Skips wherever a tool is not installed, so CI runners stay green.
    #[test]
    fn dir_fallback_agrees_with_path_lookup() {
        for (name, dirs) in [("scrcpy", scrcpy_dirs()), ("adb", adb_dirs())] {
            let (Some(via_path), Some(via_dirs)) =
                (find_in_dirs(name, path_dirs()), find_in_dirs(name, dirs))
            else {
                continue;
            };
            assert_eq!(
                via_path, via_dirs,
                "{name}: GUI fallback must pick the same binary as PATH"
            );
        }
    }

    #[test]
    fn non_executable_override_is_ignored() {
        // A stale override must not shadow a working PATH lookup.
        #[cfg(unix)]
        {
            let bad = PathBuf::from("/definitely/not/here/adb");
            let got = resolve_with("sh", "ADB_PATH", Some(bad.clone()), vec![]);
            assert!(got.is_some(), "should fall through to PATH");
            assert_ne!(got.unwrap(), bad);
        }
    }

    #[test]
    fn executable_override_wins_over_path() {
        #[cfg(unix)]
        {
            let got = resolve_with(
                "scrcpy",
                "SCRCPY_PATH",
                Some(PathBuf::from("/bin/sh")),
                vec![],
            );
            assert_eq!(got, Some(PathBuf::from("/bin/sh")));
        }
    }
}
