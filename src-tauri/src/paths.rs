use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const AUTH_KEYS: &[&str] = &[
    "cursorAuth/accessToken",
    "cursorAuth/refreshToken",
    "cursorAuth/cachedEmail",
    "cursorAuth/cachedSignUpType",
    "cursorAuth/stripeMembershipType",
    "cursorAuth/stripeSubscriptionStatus",
    "cursorAuth/cachedScopedProfile",
];

/// Inactive Application Support folder: `Cursor.multi-cursor.<envId>`
pub const DATA_INACTIVE_PREFIX: &str = "Cursor.multi-cursor.";
/// Inactive home folder: `.cursor.multi-cursor.<envId>`
pub const DOT_INACTIVE_PREFIX: &str = ".cursor.multi-cursor.";
const SWAP_TOKEN: &str = "__swap__";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Environment {
    pub id: String,
    pub name: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Account {
    pub id: String,
    pub env_id: String,
    /// Automatic name from the Cursor profile / email; refreshed on each load.
    pub name: String,
    /// Custom name set by the user; shown instead of `name` when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    pub updated_at: String,
    #[serde(default)]
    pub pending_login: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ActiveSelection {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct AppConfig {
    pub environments: Vec<Environment>,
    pub accounts: Vec<Account>,
    #[serde(default)]
    pub active: ActiveSelection,
    /// Where Cursor is installed. `None` means auto-detect for this OS.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor_app_path: Option<String>,
}

impl AppConfig {
    /// How to start Cursor: the configured location, or the detected default
    /// (with any arguments its desktop entry asks for) for this OS.
    pub fn cursor_launch(&self) -> crate::platform::CursorLaunch {
        self.cursor_app_path
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(crate::platform::CursorLaunch::program)
            .unwrap_or_else(crate::platform::default_cursor_launch)
    }
}

pub fn home_dir() -> Result<PathBuf, String> {
    crate::platform::home_dir()
}

pub fn root_dir() -> Result<PathBuf, String> {
    Ok(home_dir()?.join(".multi-cursor"))
}

pub fn config_path() -> Result<PathBuf, String> {
    Ok(root_dir()?.join("config.json"))
}

/// Parent folder of Cursor's user-data dir (Application Support / ~/.config / %APPDATA%).
pub fn application_support_dir() -> Result<PathBuf, String> {
    crate::platform::cursor_data_parent()
}

/// Active Cursor user-data dir (what normal launches use).
pub fn active_data_dir() -> Result<PathBuf, String> {
    Ok(application_support_dir()?.join(crate::platform::CURSOR_DATA_DIR_NAME))
}

/// Inactive pool entry for an environment's user-data dir.
pub fn inactive_data_dir(env_id: &str) -> Result<PathBuf, String> {
    Ok(application_support_dir()?.join(format!("{DATA_INACTIVE_PREFIX}{env_id}")))
}

/// Active `~/.cursor` (extensions / CLI state for Dock launches).
pub fn active_dot_cursor_dir() -> Result<PathBuf, String> {
    Ok(home_dir()?.join(".cursor"))
}

/// Inactive pool entry for an environment's `~/.cursor`.
pub fn inactive_dot_cursor_dir(env_id: &str) -> Result<PathBuf, String> {
    Ok(home_dir()?.join(format!("{DOT_INACTIVE_PREFIX}{env_id}")))
}

/// Resolve the user-data dir for an env (active name or inactive pool).
pub fn env_data_dir(env_id: &str, active_env_id: Option<&str>) -> Result<PathBuf, String> {
    if active_env_id == Some(env_id) {
        active_data_dir()
    } else {
        inactive_data_dir(env_id)
    }
}

pub fn env_dot_cursor_dir(env_id: &str, active_env_id: Option<&str>) -> Result<PathBuf, String> {
    if active_env_id == Some(env_id) {
        active_dot_cursor_dir()
    } else {
        inactive_dot_cursor_dir(env_id)
    }
}

pub fn env_state_db(env_id: &str, active_env_id: Option<&str>) -> Result<PathBuf, String> {
    Ok(env_data_dir(env_id, active_env_id)?
        .join("User")
        .join("globalStorage")
        .join("state.vscdb"))
}

pub fn account_snapshot_path(env_id: &str, account_id: &str) -> Result<PathBuf, String> {
    Ok(root_dir()?
        .join("accounts")
        .join(env_id)
        .join(format!("{account_id}.json")))
}

pub fn ensure_layout() -> Result<(), String> {
    let root = root_dir()?;
    fs::create_dir_all(root.join("accounts")).map_err(|e| e.to_string())?;
    Ok(())
}

pub fn load_config() -> Result<AppConfig, String> {
    ensure_layout()?;
    let path = config_path()?;
    if !path.exists() {
        let cfg = AppConfig::default();
        save_config(&cfg)?;
        return Ok(cfg);
    }
    let raw = fs::read_to_string(&path).map_err(|e| e.to_string())?;
    let mut cfg: AppConfig =
        serde_json::from_str(&raw).map_err(|e| format!("Invalid config.json: {e}"))?;
    // Older builds always stored the macOS default; let other OSes auto-detect.
    if !cfg!(target_os = "macos")
        && cfg.cursor_app_path.as_deref() == Some(crate::platform::LEGACY_MACOS_APP_PATH)
    {
        cfg.cursor_app_path = None;
    }
    Ok(cfg)
}

pub fn save_config(cfg: &AppConfig) -> Result<(), String> {
    ensure_layout()?;
    let path = config_path()?;
    let raw = serde_json::to_string_pretty(cfg).map_err(|e| e.to_string())?;
    // Write a temp file and rename it over config.json, so a crash mid-write
    // leaves the previous config intact instead of a truncated file.
    // (rename replaces the target atomically on Unix and on Windows.)
    let tmp = path.with_extension("json.tmp");
    {
        use std::io::Write;
        let mut file = fs::File::create(&tmp).map_err(|e| format!("Save config: {e}"))?;
        file.write_all(raw.as_bytes())
            .and_then(|_| file.sync_all())
            .map_err(|e| format!("Save config: {e}"))?;
    }
    fs::rename(&tmp, &path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        format!("Save config: {e}")
    })
}

pub fn now_iso() -> String {
    chrono::Local::now().to_rfc3339()
}

pub fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Ensure skeleton dirs exist for an inactive environment pool entry.
pub fn prepare_inactive_environment(env_id: &str) -> Result<(), String> {
    let data = inactive_data_dir(env_id)?;
    fs::create_dir_all(data.join("User").join("globalStorage")).map_err(|e| e.to_string())?;
    fs::create_dir_all(inactive_dot_cursor_dir(env_id)?).map_err(|e| e.to_string())?;
    fs::create_dir_all(root_dir()?.join("accounts").join(env_id)).map_err(|e| e.to_string())?;
    Ok(())
}

fn storage_v2_marker() -> Result<PathBuf, String> {
    Ok(root_dir()?.join(".storage-v2"))
}

/// One-time migration from `--user-data-dir` under `~/.multi-cursor/environments`.
pub fn migrate_legacy_storage(cfg: &AppConfig) -> Result<(), String> {
    let marker = storage_v2_marker()?;
    if marker.exists() {
        return Ok(());
    }

    if let Some(active_id) = cfg.active.env_id.as_deref() {
        let legacy = root_dir()?.join("environments").join(active_id);
        if legacy.exists() {
            let live = active_data_dir()?;
            let live_dot = active_dot_cursor_dir()?;
            let stock_id = new_id();
            let stock = format!("_pre-v2-{}", &stock_id[..8]);
            if live.exists() {
                let park = inactive_data_dir(&stock)?;
                if !park.exists() {
                    rename_path(&live, &park)?;
                }
            }
            if live_dot.exists() {
                let park_dot = inactive_dot_cursor_dir(&stock)?;
                if !park_dot.exists() {
                    rename_path(&live_dot, &park_dot)?;
                }
            }
            rename_path(&legacy, &live)?;
            let nested_ext = live.join("extensions");
            if nested_ext.exists() {
                fs::create_dir_all(&live_dot).map_err(|e| e.to_string())?;
                let dest_ext = live_dot.join("extensions");
                if dest_ext.exists() {
                    let _ = move_to_trash(&dest_ext);
                }
                rename_path(&nested_ext, &dest_ext)?;
            } else {
                fs::create_dir_all(&live_dot).map_err(|e| e.to_string())?;
            }
        }
    }

    fs::write(&marker, "2\n").map_err(|e| e.to_string())?;
    Ok(())
}

/// Ensure pool folders exist for every configured environment.
/// Migrates legacy `~/.multi-cursor/environments/<id>` when present.
pub fn ensure_environment_pool(cfg: &AppConfig) -> Result<(), String> {
    let active = cfg.active.env_id.as_deref();
    ensure_active_dirs()?;
    for env in &cfg.environments {
        fs::create_dir_all(root_dir()?.join("accounts").join(&env.id))
            .map_err(|e| e.to_string())?;
        if Some(env.id.as_str()) == active {
            continue;
        }
        let data = inactive_data_dir(&env.id)?;
        let dot = inactive_dot_cursor_dir(&env.id)?;
        if data.exists() && dot.exists() {
            continue;
        }
        let legacy = root_dir()?.join("environments").join(&env.id);
        if legacy.exists() {
            if !data.exists() {
                copy_dir_recursive(&legacy, &data)?;
            }
            if !dot.exists() {
                let legacy_ext = legacy.join("extensions");
                fs::create_dir_all(&dot).map_err(|e| e.to_string())?;
                if legacy_ext.exists() {
                    copy_dir_recursive(&legacy_ext, &dot.join("extensions"))?;
                }
            }
        } else {
            prepare_inactive_environment(&env.id)?;
        }
    }
    Ok(())
}

/// Ensure the live Cursor / ~/.cursor folders exist (active environment).
pub fn ensure_active_dirs() -> Result<(), String> {
    let data = active_data_dir()?;
    fs::create_dir_all(data.join("User").join("globalStorage")).map_err(|e| e.to_string())?;
    fs::create_dir_all(active_dot_cursor_dir()?).map_err(|e| e.to_string())?;
    Ok(())
}

pub fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<(), String> {
    if !src.exists() {
        return Err(format!("Source does not exist: {}", src.display()));
    }
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    if dst.exists() {
        fs::remove_dir_all(dst).map_err(|e| format!("Clear destination {}: {e}", dst.display()))?;
    }

    #[cfg(target_os = "macos")]
    {
        let status = std::process::Command::new("ditto")
            .arg(src)
            .arg(dst)
            .status()
            .map_err(|e| format!("ditto failed to start: {e}"))?;
        if status.success() {
            return Ok(());
        }
    }

    copy_tree_portable(src, dst)
}

fn copy_tree_portable(src: &Path, dst: &Path) -> Result<(), String> {
    fs::create_dir_all(dst).map_err(|e| e.to_string())?;
    for entry in fs::read_dir(src).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let ty = entry.file_type().map_err(|e| e.to_string())?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if ty.is_symlink() {
            copy_symlink(&from, &to)?;
        } else if ty.is_dir() {
            copy_tree_portable(&from, &to)?;
        } else if ty.is_file() {
            fs::copy(&from, &to).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

/// Recreate a symlink (Unix), or copy what it points to (Windows, where
/// creating symlinks usually needs elevated rights).
pub fn copy_symlink(from: &Path, to: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        let target = fs::read_link(from).map_err(|e| e.to_string())?;
        std::os::unix::fs::symlink(&target, to)
            .map_err(|e| format!("Symlink {}: {e}", to.display()))
    }
    #[cfg(not(unix))]
    {
        let meta = match fs::metadata(from) {
            Ok(m) => m,
            Err(_) => return Ok(()), // dangling link: skip
        };
        if meta.is_dir() {
            copy_tree_portable(from, to)
        } else {
            fs::copy(from, to).map(|_| ()).map_err(|e| e.to_string())
        }
    }
}

fn rename_path(from: &Path, to: &Path) -> Result<(), String> {
    if to.exists() {
        return Err(format!(
            "Cannot rename {} → {} (destination exists)",
            from.display(),
            to.display()
        ));
    }
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    rename_with_retry(from, to).map_err(|e| {
        let hint = if is_in_use_error(&e) {
            " (a file inside is still in use — make sure Cursor has fully quit)"
        } else {
            ""
        };
        format!(
            "Failed to rename {} → {}: {e}{hint}",
            from.display(),
            to.display()
        )
    })
}

/// On Windows a directory can't be renamed while any file inside is open
/// (lingering Cursor helpers, antivirus, the search indexer). Those handles
/// usually go away within a few seconds, so retry briefly.
fn rename_with_retry(from: &Path, to: &Path) -> std::io::Result<()> {
    const ATTEMPTS: u32 = if cfg!(windows) { 15 } else { 1 };
    let mut last = None;
    for attempt in 0..ATTEMPTS {
        match fs::rename(from, to) {
            Ok(()) => return Ok(()),
            Err(e) if is_in_use_error(&e) && attempt + 1 < ATTEMPTS => {
                last = Some(e);
                std::thread::sleep(std::time::Duration::from_millis(300));
            }
            Err(e) => return Err(e),
        }
    }
    Err(last.unwrap_or_else(|| std::io::Error::other("rename failed")))
}

fn is_in_use_error(e: &std::io::Error) -> bool {
    if e.kind() == std::io::ErrorKind::PermissionDenied {
        return true;
    }
    // ERROR_SHARING_VIOLATION (32) / ERROR_LOCK_VIOLATION (33) on Windows.
    cfg!(windows) && matches!(e.raw_os_error(), Some(32) | Some(33))
}

/// Atomically (as far as rename allows) make `to_env_id` the live Cursor / ~/.cursor.
///
/// `from_env_id` is the currently active environment id (its data is under the
/// live names). After success, `to_env_id` owns the live names and `from_env_id`
/// sits in the inactive pool.
pub fn activate_environment_dirs(
    from_env_id: Option<&str>,
    to_env_id: &str,
) -> Result<(), String> {
    if from_env_id == Some(to_env_id) {
        return Ok(());
    }

    let to_data = inactive_data_dir(to_env_id)?;
    let to_dot = inactive_dot_cursor_dir(to_env_id)?;
    if !to_data.exists() {
        return Err(format!(
            "Environment data folder missing: {}",
            to_data.display()
        ));
    }
    if !to_dot.exists() {
        return Err(format!(
            "Environment ~/.cursor folder missing: {}",
            to_dot.display()
        ));
    }

    let active_data = active_data_dir()?;
    let active_dot = active_dot_cursor_dir()?;

    // 1) Move live dirs aside (to previous env's inactive names, or temp).
    let (park_data, park_dot) = if let Some(from_id) = from_env_id {
        let dest_data = inactive_data_dir(from_id)?;
        let dest_dot = inactive_dot_cursor_dir(from_id)?;
        if dest_data.exists() {
            return Err(format!(
                "Inactive data folder already exists for active env: {}",
                dest_data.display()
            ));
        }
        if dest_dot.exists() {
            return Err(format!(
                "Inactive ~/.cursor folder already exists for active env: {}",
                dest_dot.display()
            ));
        }
        (dest_data, dest_dot)
    } else {
        let swap_suffix = format!("{SWAP_TOKEN}-{}", new_id());
        (
            application_support_dir()?.join(format!("{DATA_INACTIVE_PREFIX}{swap_suffix}")),
            home_dir()?.join(format!("{DOT_INACTIVE_PREFIX}{swap_suffix}")),
        )
    };

    // Each completed rename is recorded so a later failure can undo it and
    // leave the live folders exactly as they were.
    let mut done: Vec<(PathBuf, PathBuf)> = Vec::new();
    let result = (|| -> Result<(), String> {
        if active_data.exists() {
            rename_path(&active_data, &park_data)?;
            done.push((active_data.clone(), park_data.clone()));
        } else {
            fs::create_dir_all(park_data.join("User").join("globalStorage"))
                .map_err(|e| e.to_string())?;
        }
        if active_dot.exists() {
            rename_path(&active_dot, &park_dot)?;
            done.push((active_dot.clone(), park_dot.clone()));
        } else {
            fs::create_dir_all(&park_dot).map_err(|e| e.to_string())?;
        }

        // 2) Promote target inactive dirs to live names.
        rename_path(&to_data, &active_data)?;
        done.push((to_data.clone(), active_data.clone()));
        rename_path(&to_dot, &active_dot)?;
        Ok(())
    })();

    if let Err(err) = result {
        for (orig, moved) in done.into_iter().rev() {
            if let Err(undo) = rename_path(&moved, &orig) {
                return Err(format!("{err}. Rolling back also failed: {undo}"));
            }
        }
        return Err(err);
    }
    Ok(())
}

pub fn move_to_trash(path: &Path) -> Result<(), String> {
    if !path.exists() {
        return Ok(());
    }
    #[cfg(not(target_os = "macos"))]
    {
        trash::delete(path).map_err(|e| {
            format!(
                "Failed to move {} to the {}: {e}",
                path.display(),
                crate::platform::trash_name()
            )
        })
    }

    #[cfg(target_os = "macos")]
    {
        // NSFileManager needs no permission prompt. The trash crate's default
        // (Finder via AppleScript) asks to control Finder, so keep that as
        // the fallback only.
        use trash::macos::{DeleteMethod, TrashContextExtMacos};
        let mut ctx = trash::TrashContext::default();
        ctx.set_delete_method(DeleteMethod::NsFileManager);
        match ctx.delete(path) {
            Ok(()) => Ok(()),
            Err(_) => move_to_trash_finder(path),
        }
    }
}

#[cfg(target_os = "macos")]
fn move_to_trash_finder(path: &Path) -> Result<(), String> {
    let posix = path
        .canonicalize()
        .unwrap_or_else(|_| path.to_path_buf())
        .display()
        .to_string();
    let script = format!(
        "tell application \"Finder\" to delete (POSIX file \"{}\" as alias)",
        posix.replace('\\', "\\\\").replace('"', "\\\"")
    );
    let status = std::process::Command::new("osascript")
        .arg("-e")
        .arg(script)
        .status()
        .map_err(|e| e.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("Failed to move to Trash: {}", path.display()))
    }
}

/// Serialises sandboxed tests and points every home / Cursor data path at a
/// fresh temp dir until dropped, on every OS (see `platform::TEST_ROOT`).
#[cfg(test)]
pub(crate) struct SandboxGuard {
    _lock: std::sync::MutexGuard<'static, ()>,
}

#[cfg(test)]
impl Drop for SandboxGuard {
    fn drop(&mut self) {
        *crate::platform::TEST_ROOT.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

/// Fresh temp root for a test; real Cursor data is never touched.
#[cfg(test)]
pub(crate) fn test_sandbox() -> (SandboxGuard, PathBuf) {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let lock = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = std::env::temp_dir().join(format!("multi-cursor-test-{}", new_id()));
    fs::create_dir_all(&dir).unwrap();
    *crate::platform::TEST_ROOT.lock().unwrap_or_else(|e| e.into_inner()) = Some(dir.clone());
    // Fail loudly rather than ever running against real data.
    assert!(home_dir().unwrap().starts_with(&dir));
    assert!(active_data_dir().unwrap().starts_with(&dir));
    (SandboxGuard { _lock: lock }, dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switch_environment_dirs() {
        let (_guard, root) = test_sandbox();
        assert!(active_data_dir().unwrap().starts_with(&root));

        // Live env "a" with a marker file; inactive env "b" in the pool.
        ensure_active_dirs().unwrap();
        fs::write(active_data_dir().unwrap().join("marker"), "a").unwrap();
        prepare_inactive_environment("b").unwrap();
        fs::write(inactive_data_dir("b").unwrap().join("marker"), "b").unwrap();

        activate_environment_dirs(Some("a"), "b").unwrap();
        let live_marker = active_data_dir().unwrap().join("marker");
        assert_eq!(fs::read_to_string(&live_marker).unwrap(), "b");
        assert_eq!(
            fs::read_to_string(inactive_data_dir("a").unwrap().join("marker")).unwrap(),
            "a"
        );
        assert!(inactive_dot_cursor_dir("a").unwrap().exists());

        // Target "c" has data but no ~/.cursor pool entry → precondition error, nothing moved.
        prepare_inactive_environment("c").unwrap();
        fs::remove_dir_all(inactive_dot_cursor_dir("c").unwrap()).unwrap();
        assert!(activate_environment_dirs(Some("b"), "c").is_err());
        assert_eq!(fs::read_to_string(&live_marker).unwrap(), "b");

        // Trash works on this platform.
        let junk = root.join("junk");
        fs::create_dir_all(&junk).unwrap();
        move_to_trash(&junk).unwrap();
        assert!(!junk.exists());

        let _ = fs::remove_dir_all(&root);
    }
}
