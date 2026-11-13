use std::path::Path;
use std::process::{Command, Stdio};

// ---------------------------------------------------------------------------
// macOS
// ---------------------------------------------------------------------------

/// True when the Cursor IDE main process is running.
///
/// Uses `killall -s` (dry-run) matching the exact process name `Cursor`.
/// Prefer this over `pgrep -f …/MacOS/Cursor`: on recent macOS, `pgrep` often
/// cannot see the Electron main process even when Cursor is running, while
/// `killall` still can. Helper processes are named `Cursor Helper*` and are
/// not matched. Multi Cursor's own binary is `multi-cursor`, so it is safe.
#[cfg(target_os = "macos")]
pub fn cursor_running() -> bool {
    let output = Command::new("killall")
        .args(["-s", "Cursor"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output();
    match output {
        Ok(out) if out.status.success() => {
            let text = String::from_utf8_lossy(&out.stdout);
            // Dry-run lines look like: `kill -term 12345`
            text.lines().any(|l| l.contains("kill "))
        }
        _ => false,
    }
}

/// Ask Cursor to quit; returns immediately without waiting for exit.
#[cfg(target_os = "macos")]
pub fn request_quit_cursor() -> Result<(), String> {
    let _ = Command::new("osascript")
        .args(["-e", "tell application \"Cursor\" to quit"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    Ok(())
}

/// Force-kill Cursor processes (non-blocking spawn).
#[cfg(target_os = "macos")]
pub fn force_quit_cursor() -> Result<(), String> {
    let _ = Command::new("killall")
        .arg("Cursor")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    Ok(())
}

/// Launch Cursor with its default paths (Application Support/Cursor + ~/.cursor).
#[cfg(target_os = "macos")]
pub fn launch_cursor(launch: &crate::platform::CursorLaunch) -> Result<(), String> {
    let app_path = launch.program.as_str();
    if !Path::new(app_path).exists() {
        return Err(format!("Cursor app not found at {app_path}"));
    }
    // `open` returns as soon as Launch Services accepts the request.
    let status = Command::new("open")
        .arg(app_path)
        .status()
        .map_err(|e| format!("Failed to launch Cursor: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err("open failed to launch Cursor".to_string())
    }
}

// ---------------------------------------------------------------------------
// Linux / Windows
// ---------------------------------------------------------------------------

#[cfg(not(target_os = "macos"))]
use sysinfo::{
    Pid, Process, ProcessRefreshKind, ProcessStatus, ProcessesToUpdate, System, UpdateKind,
};

/// All Cursor processes (main + Electron helpers, which share the same exe
/// name on Linux and Windows), excluding Multi Cursor itself.
#[cfg(not(target_os = "macos"))]
fn with_cursor_processes<R>(f: impl FnOnce(Vec<&Process>) -> R) -> R {
    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing().with_exe(UpdateKind::OnlyIfNotSet),
    );
    let own = Pid::from_u32(std::process::id());
    let procs = sys
        .processes()
        .values()
        .filter(|p| p.pid() != own)
        // An exited process whose parent hasn't collected it yet is not running.
        .filter(|p| !matches!(p.status(), ProcessStatus::Zombie | ProcessStatus::Dead))
        .filter(|p| crate::platform::is_cursor_main_process(&p.name().to_string_lossy(), p.exe()))
        .collect();
    f(procs)
}

/// Split Cursor processes into main processes (parent is not Cursor) and
/// their children. Children include Electron helpers (`--type=…`) and
/// Node-mode processes Cursor runs with its own binary (extension host,
/// agent) that carry no `--type=` flag, so the parent is the reliable signal.
#[cfg(not(target_os = "macos"))]
fn split_main(procs: Vec<&Process>) -> (Vec<&Process>, Vec<&Process>) {
    let pids: std::collections::HashSet<Pid> = procs.iter().map(|p| p.pid()).collect();
    procs
        .into_iter()
        .partition(|p| p.parent().is_none_or(|parent| !pids.contains(&parent)))
}

/// True while any Cursor process is alive. Helpers are included on purpose:
/// they can keep files in the data folder open, which blocks the folder
/// rename on Windows.
#[cfg(not(target_os = "macos"))]
pub fn cursor_running() -> bool {
    with_cursor_processes(|procs| !procs.is_empty())
}

/// Ask Cursor to quit; returns immediately without waiting for exit.
#[cfg(target_os = "windows")]
pub fn request_quit_cursor() -> Result<(), String> {
    // Without /F, taskkill posts WM_CLOSE to the windows, which Electron
    // handles like the user closing them (unsaved-changes prompts included).
    let _ = hidden_command("taskkill")
        .args(["/IM", "Cursor.exe"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    Ok(())
}

/// Ask Cursor to quit; returns immediately without waiting for exit.
#[cfg(all(unix, not(target_os = "macos")))]
pub fn request_quit_cursor() -> Result<(), String> {
    // SIGTERM to the main process only; Electron then closes its windows and
    // children. Signalling helpers directly can make Cursor show crash UI.
    with_cursor_processes(|procs| {
        let (main, _children) = split_main(procs);
        for p in main {
            let _ = p.kill_with(sysinfo::Signal::Term);
        }
    });
    Ok(())
}

/// Force-kill all Cursor processes.
#[cfg(not(target_os = "macos"))]
pub fn force_quit_cursor() -> Result<(), String> {
    with_cursor_processes(|procs| {
        // Main first so it doesn't respawn helpers, then everything left.
        let (main, children) = split_main(procs);
        for p in main.into_iter().chain(children) {
            let _ = p.kill();
        }
    });
    Ok(())
}

/// Launch Cursor with its default data paths, detached from Multi Cursor.
#[cfg(not(target_os = "macos"))]
pub fn launch_cursor(launch: &crate::platform::CursorLaunch) -> Result<(), String> {
    let app_path = launch.program.as_str();
    if !Path::new(app_path).is_file() {
        return Err(format!(
            "Cursor not found at {app_path}. Set the Cursor location in ~/.multi-cursor/config.json (cursorAppPath)."
        ));
    }
    // Cursor is a GUI app: no console flag needed (DETACHED_PROCESS below).
    let mut cmd = Command::new(app_path);
    cmd.args(&launch.args);
    #[cfg(all(unix, not(target_os = "macos")))]
    strip_appimage_env(&mut cmd);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(dir) = Path::new(app_path).parent() {
        cmd.current_dir(dir);
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        use std::os::unix::process::CommandExt;
        // New process group so Cursor survives Multi Cursor quitting and
        // doesn't receive our terminal's signals.
        cmd.process_group(0);
    }
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("Failed to launch Cursor: {e}"))?;
    // Cursor is our child process. Wait for it in the background so that when
    // it quits it is reaped instead of lingering as a zombie that still looks
    // like a running "cursor" process.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// Environment variables only an AppImage runtime sets.
#[cfg(all(unix, not(target_os = "macos")))]
const APPIMAGE_ONLY_VARS: &[&str] = &[
    "APPDIR",
    "APPIMAGE",
    "ARGV0",
    "OWD",
    "GTK_THEME",
    "PYTHONDONTWRITEBYTECODE",
];

/// When Multi Cursor runs as an AppImage, its runtime points LD_LIBRARY_PATH,
/// PATH, GTK/GIO/GStreamer/Qt/Python module paths, XDG_DATA_DIRS… into the
/// AppImage mount. Cursor must not inherit those: it would load Multi
/// Cursor's bundled libraries, and the mount disappears when we quit.
#[cfg(all(unix, not(target_os = "macos")))]
fn strip_appimage_env(cmd: &mut Command) {
    let Some(appdir) = std::env::var("APPDIR").ok() else {
        return;
    };
    let vars = std::env::vars_os()
        .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)));
    for (key, value) in appimage_env_fixes(&appdir, vars) {
        match value {
            Some(v) => cmd.env(key, v),
            None => cmd.env_remove(key),
        };
    }
}

/// Changes to undo an AppImage runtime's environment: `(name, None)` removes
/// a variable, `(name, Some(v))` sets it. Path-list entries under `appdir`
/// are dropped (and the variable removed if nothing is left); the
/// AppImage-only variables are removed.
#[cfg(all(unix, not(target_os = "macos")))]
fn appimage_env_fixes(
    appdir: &str,
    vars: impl Iterator<Item = (String, String)>,
) -> Vec<(String, Option<String>)> {
    let appdir = appdir.trim_end_matches('/');
    if appdir.is_empty() {
        return Vec::new();
    }
    let mut fixes = Vec::new();
    for (key, value) in vars {
        if APPIMAGE_ONLY_VARS.contains(&key.as_str()) {
            fixes.push((key, None));
            continue;
        }
        if !value.contains(appdir) {
            continue;
        }
        let kept: Vec<&str> = value
            .split(':')
            .filter(|part| !part.is_empty() && !part.starts_with(appdir))
            .collect();
        fixes.push((key, (!kept.is_empty()).then(|| kept.join(":"))));
    }
    fixes
}

#[cfg(all(test, unix, not(target_os = "macos")))]
mod tests {
    use super::*;

    #[test]
    fn appimage_environment_is_undone() {
        // Captured from the real Multi Cursor AppImage (mount path shortened).
        let m = "/tmp/.mount_MultiX";
        let vars = [
            ("APPDIR", m.to_string()),
            ("APPIMAGE", "/home/me/Multi.AppImage".into()),
            (
                "LD_LIBRARY_PATH",
                format!("{m}/usr/lib/:{m}/usr/lib/x86_64-linux-gnu/:"),
            ),
            (
                "PATH",
                format!("{m}/usr/bin/:{m}/bin/:/home/me/.local/bin:/usr/bin"),
            ),
            (
                "XDG_DATA_DIRS",
                format!("{m}/usr/share/:{m}/usr/share:/usr/share:"),
            ),
            ("GTK_PATH", format!("{m}//usr/lib/gtk-3.0")),
            ("GTK_THEME", "Adwaita:light".into()),
            ("PYTHONHOME", format!("{m}/usr/")),
            ("HOME", "/home/me".into()),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v));
        let mut fixes = appimage_env_fixes(m, vars);
        fixes.sort();
        let get = |k: &str| fixes.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        assert_eq!(get("APPDIR"), Some(None));
        assert_eq!(get("APPIMAGE"), Some(None));
        assert_eq!(get("GTK_THEME"), Some(None));
        assert_eq!(get("LD_LIBRARY_PATH"), Some(None));
        assert_eq!(get("GTK_PATH"), Some(None));
        assert_eq!(get("PYTHONHOME"), Some(None));
        assert_eq!(
            get("PATH"),
            Some(Some("/home/me/.local/bin:/usr/bin".into()))
        );
        assert_eq!(get("XDG_DATA_DIRS"), Some(Some("/usr/share".into())));
        assert_eq!(get("HOME"), None, "unrelated variables are left alone");
    }
}

/// `Command` that never flashes a console window on Windows.
#[cfg(target_os = "windows")]
fn hidden_command(program: &str) -> Command {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let mut cmd = Command::new(program);
    cmd.creation_flags(CREATE_NO_WINDOW);
    cmd
}
