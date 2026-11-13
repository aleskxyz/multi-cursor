use serde::Serialize;
use tauri::AppHandle;

use crate::auth::{
    clear_auth_keys, delete_snapshot, display_name_from_snapshot, email_from_snapshot,
    has_login_tokens, load_snapshot, profile_name_from_snapshot, read_auth_keys, save_snapshot,
    write_auth_keys, AuthSnapshot,
};
use crate::copy_progress::{
    cleanup_partial_copy, copy_cursor_tree_with_progress, dir_size_bytes, emit_progress,
    purge_vscdb_bak_files, trash_paths_with_progress,
};
use crate::cursor::{cursor_running, force_quit_cursor, launch_cursor, request_quit_cursor};
use crate::paths::{
    activate_environment_dirs, active_data_dir, active_dot_cursor_dir, application_support_dir,
    ensure_active_dirs, ensure_environment_pool, env_data_dir, env_dot_cursor_dir, env_state_db,
    home_dir, inactive_data_dir, inactive_dot_cursor_dir, load_config, migrate_legacy_storage,
    new_id, now_iso, prepare_inactive_environment, root_dir, save_config,
    Account, AppConfig, Environment, DATA_INACTIVE_PREFIX, DOT_INACTIVE_PREFIX,
};

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppState {
    pub config: AppConfig,
    pub cursor_running: bool,
    pub root_dir: String,
    /// `macos`, `linux`, or `windows`.
    pub os: &'static str,
    /// "Trash" or "Recycle Bin".
    pub trash_name: &'static str,
}

fn state_from(cfg: AppConfig) -> Result<AppState, String> {
    Ok(AppState {
        config: cfg,
        cursor_running: cursor_running(),
        root_dir: root_dir()?.display().to_string(),
        os: crate::platform::os_id(),
        trash_name: crate::platform::trash_name(),
    })
}

fn active_env_id(cfg: &AppConfig) -> Option<&str> {
    cfg.active.env_id.as_deref()
}

fn state_db(cfg: &AppConfig, env_id: &str) -> Result<std::path::PathBuf, String> {
    env_state_db(env_id, active_env_id(cfg))
}

/// First launch: register the live Cursor / ~/.cursor folders as "Default"
/// and capture the signed-in account email when tokens are present.
fn bootstrap_default_environment(cfg: &mut AppConfig) -> Result<(), String> {
    ensure_active_dirs()?;
    fs_create_accounts_root()?;

    if !cfg.environments.is_empty() {
        if cfg.active.env_id.is_none() {
            cfg.active.env_id = cfg.environments.first().map(|e| e.id.clone());
            save_config(cfg)?;
        }
        return Ok(());
    }

    let id = new_id();
    cfg.environments.push(Environment {
        id: id.clone(),
        name: "Default".to_string(),
        created_at: now_iso(),
    });
    cfg.active.env_id = Some(id.clone());
    capture_default_account(cfg, &id)?;
    save_config(cfg)?;
    // Fresh install — no legacy --user-data-dir tree to migrate.
    let marker = root_dir()?.join(".storage-v2");
    if !marker.exists() {
        std::fs::write(&marker, "2\n").map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn adopt_live_login_if_needed(cfg: &mut AppConfig) -> Result<(), String> {
    let Some(env_id) = cfg.active.env_id.clone() else {
        return Ok(());
    };
    if cfg.accounts.iter().any(|a| a.env_id == env_id) {
        return Ok(());
    }
    capture_default_account(cfg, &env_id)?;
    save_config(cfg)?;
    Ok(())
}

fn capture_default_account(cfg: &mut AppConfig, env_id: &str) -> Result<(), String> {
    let db = env_state_db(env_id, Some(env_id))?;
    let snap = read_auth_keys(&db)?;
    if !has_login_tokens(&snap) {
        return Ok(());
    }
    let account_id = new_id();
    save_snapshot(env_id, &account_id, &snap)?;
    let email = email_from_snapshot(&snap);
    let name = display_name_from_snapshot(&snap)
        .unwrap_or_else(|| "Signed-in account".to_string());
    cfg.accounts.push(Account {
        id: account_id.clone(),
        env_id: env_id.to_string(),
        name,
        email,
        updated_at: now_iso(),
        pending_login: false,
        label: None,
    });
    cfg.active.account_id = Some(account_id);
    Ok(())
}

fn fs_create_accounts_root() -> Result<(), String> {
    std::fs::create_dir_all(root_dir()?.join("accounts")).map_err(|e| e.to_string())
}

fn account_needs_identity(account: &Account) -> bool {
    account.pending_login
        || account.email.as_ref().map(|e| e.is_empty()).unwrap_or(true)
        || account.name.starts_with("Signing in")
}

fn apply_captured_identity(account: &mut Account, email: &str, snap: &AuthSnapshot) {
    account.pending_login = false;
    account.email = Some(email.to_string());
    account.name = profile_name_from_snapshot(snap)
        .unwrap_or_else(|| email.to_string());
    account.updated_at = now_iso();
}

fn refresh_account_identity_from_snapshot(account: &mut Account) {
    if account.pending_login {
        return;
    }
    let Ok(snap) = load_snapshot(&account.env_id, &account.id) else {
        return;
    };
    if let Some(email) = email_from_snapshot(&snap) {
        account.email = Some(email.clone());
        account.name = profile_name_from_snapshot(&snap).unwrap_or(email);
    } else if let Some(name) = profile_name_from_snapshot(&snap) {
        account.name = name;
    }
}

fn try_capture_pending(cfg: &mut AppConfig) -> Result<Vec<String>, String> {
    // Capture pending logins. Also heal accounts stuck after an earlier capture
    // that saw tokens before Cursor wrote cachedEmail (pending cleared, name left).
    let candidates: Vec<(String, String, bool)> = cfg
        .accounts
        .iter()
        .filter(|a| account_needs_identity(a))
        .map(|a| (a.env_id.clone(), a.id.clone(), a.pending_login))
        .collect();

    let mut captured_emails = Vec::new();
    for (env_id, account_id, was_pending) in candidates {
        if was_pending {
            // The live DB belongs to whichever account is active. A pending
            // account that is no longer active was abandoned (login window
            // closed, another account launched); reading the live DB for it
            // would copy someone else's login into it.
            let is_active = cfg.active.env_id.as_deref() == Some(env_id.as_str())
                && cfg.active.account_id.as_deref() == Some(account_id.as_str());
            if !is_active {
                discard_account(cfg, &account_id)?;
                continue;
            }
            let db = state_db(cfg, &env_id)?;
            let snap = match read_auth_keys(&db) {
                Ok(s) => s,
                Err(_) => continue,
            };
            if !has_login_tokens(&snap) {
                continue;
            }
            let Some(email) = email_from_snapshot(&snap) else {
                // Tokens can appear before cachedEmail — keep waiting.
                continue;
            };
            // Signed in as an account this environment already has: refresh
            // that account's snapshot instead of creating a duplicate.
            let existing = cfg
                .accounts
                .iter()
                .find(|a| {
                    a.id != account_id
                        && a.env_id == env_id
                        && !a.pending_login
                        && a.email
                            .as_deref()
                            .is_some_and(|e| e.eq_ignore_ascii_case(&email))
                })
                .map(|a| a.id.clone());
            let target_id = match existing {
                Some(existing_id) => {
                    // Keep a custom name given to the sign-in if the existing
                    // account has none.
                    let pending_label = cfg
                        .accounts
                        .iter()
                        .find(|a| a.id == account_id)
                        .and_then(|a| a.label.clone());
                    if let Some(existing) = cfg.accounts.iter_mut().find(|a| a.id == existing_id) {
                        if existing.label.is_none() {
                            existing.label = pending_label;
                        }
                    }
                    discard_account(cfg, &account_id)?;
                    existing_id
                }
                None => account_id,
            };
            save_snapshot(&env_id, &target_id, &snap)?;
            if let Some(account) = cfg.accounts.iter_mut().find(|a| a.id == target_id) {
                apply_captured_identity(account, &email, &snap);
            }
            captured_emails.push(email);
            cfg.active.env_id = Some(env_id);
            cfg.active.account_id = Some(target_id);
            continue;
        }

        // Non-pending heal: only trust this account's own snapshot (not the live DB,
        // which may belong to a different account in the same environment).
        let Ok(snap) = load_snapshot(&env_id, &account_id) else {
            continue;
        };
        let Some(email) = email_from_snapshot(&snap) else {
            continue;
        };
        if !has_login_tokens(&snap) {
            continue;
        }
        if let Some(account) = cfg.accounts.iter_mut().find(|a| a.id == account_id) {
            apply_captured_identity(account, &email, &snap);
        }
    }
    Ok(captured_emails)
}

/// Remove an account entry and its snapshot.
fn discard_account(cfg: &mut AppConfig, account_id: &str) -> Result<(), String> {
    if let Some(account) = cfg.accounts.iter().find(|a| a.id == account_id).cloned() {
        delete_snapshot(&account.env_id, &account.id)?;
        cfg.accounts.retain(|a| a.id != account_id);
        if cfg.active.account_id.as_deref() == Some(account_id) {
            cfg.active.account_id = None;
        }
    }
    Ok(())
}

/// Drop pending ("Signing in…") accounts that never captured a login, except
/// `keep`. Called whenever another account or environment becomes active, so
/// an abandoned sign-in can't later pick up a different account's login.
fn discard_abandoned_pending(cfg: &mut AppConfig, keep: Option<&str>) -> Result<(), String> {
    // A sign-in may have completed since the UI last polled; capture it now,
    // before the live DB gets overwritten by the next account.
    try_capture_pending(cfg)?;
    let abandoned: Vec<String> = cfg
        .accounts
        .iter()
        .filter(|a| a.pending_login && Some(a.id.as_str()) != keep)
        .map(|a| a.id.clone())
        .collect();
    for id in abandoned {
        discard_account(cfg, &id)?;
    }
    Ok(())
}

fn save_active_account_tokens(cfg: &AppConfig, env_id: &str) -> Result<(), String> {
    let Some(account_id) = cfg.active.account_id.as_deref() else {
        return Ok(());
    };
    let Some(account) = cfg.accounts.iter().find(|a| a.id == account_id) else {
        return Ok(());
    };
    if account.env_id != env_id || account.pending_login {
        return Ok(());
    }
    let db = state_db(cfg, env_id)?;
    let snap = read_auth_keys(&db)?;
    if has_login_tokens(&snap) {
        save_snapshot(env_id, account_id, &snap)?;
    }
    Ok(())
}

/// Make `env_id` own the live Application Support/Cursor and ~/.cursor folders.
fn switch_environment(cfg: &mut AppConfig, env_id: &str) -> Result<(), String> {
    if !cfg.environments.iter().any(|e| e.id == env_id) {
        return Err("Environment not found".to_string());
    }
    if cfg.active.env_id.as_deref() == Some(env_id) {
        return Ok(());
    }
    if cursor_running() {
        return Err(
            "Cursor is still running. Wait for it to quit before switching environments."
                .to_string(),
        );
    }

    if let Some(current) = cfg.active.env_id.clone() {
        save_active_account_tokens(cfg, &current)?;
    }

    activate_environment_dirs(cfg.active.env_id.as_deref(), env_id)?;
    cfg.active.env_id = Some(env_id.to_string());
    // Account may belong to the previous env; clear until apply_account sets it.
    if cfg
        .active
        .account_id
        .as_ref()
        .and_then(|aid| cfg.accounts.iter().find(|a| a.id == *aid))
        .map(|a| a.env_id.as_str())
        != Some(env_id)
    {
        cfg.active.account_id = None;
    }
    save_config(cfg)?;
    Ok(())
}

/// Switch auth in the environment DB. Caller must ensure Cursor is not running.
fn apply_account(cfg: &mut AppConfig, env_id: &str, account_id: &str) -> Result<(), String> {
    if !cfg
        .accounts
        .iter()
        .any(|a| a.id == account_id && a.env_id == env_id)
    {
        return Err("Account not found".to_string());
    }

    // This may capture a sign-in that finished since the last poll, possibly
    // *for this very account*: it can stop being pending, or be merged into an
    // existing account with the same email (and removed). Look it up again
    // afterwards instead of trusting a copy taken before.
    discard_abandoned_pending(cfg, Some(account_id))?;
    let account_id = if cfg.accounts.iter().any(|a| a.id == account_id) {
        account_id.to_string()
    } else if cfg.active.env_id.as_deref() == Some(env_id) {
        cfg.active
            .account_id
            .clone()
            .ok_or_else(|| "Account not found".to_string())?
    } else {
        return Err("Account not found".to_string());
    };
    let account_id = account_id.as_str();
    let pending = cfg
        .accounts
        .iter()
        .find(|a| a.id == account_id)
        .is_some_and(|a| a.pending_login);

    switch_environment(cfg, env_id)?;
    save_active_account_tokens(cfg, env_id)?;

    let db = state_db(cfg, env_id)?;
    if pending {
        clear_auth_keys(&db)?;
    } else {
        let snap = load_snapshot(env_id, account_id)?;
        write_auth_keys(&db, &snap)?;
    }

    cfg.active.env_id = Some(env_id.to_string());
    cfg.active.account_id = Some(account_id.to_string());
    save_config(cfg)?;
    Ok(())
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ListStateResult {
    pub state: AppState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub captured_email: Option<String>,
    /// Environments rebuilt from pool folders the config didn't know about.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub recovered: Vec<String>,
}

#[tauri::command]
pub fn list_state() -> Result<ListStateResult, String> {
    let mut cfg = load_config()?;
    bootstrap_default_environment(&mut cfg)?;
    migrate_legacy_storage(&cfg)?;
    // Before ensure_environment_pool, so a recovered environment that is
    // missing one of its two folders gets an empty one created right away.
    let recovered = recover_orphan_pool_dirs(&mut cfg)?;
    ensure_environment_pool(&cfg)?;
    // Reclaim disk from older Multi Cursor builds that full-copied state.vscdb.
    let _ = purge_vscdb_bak_files(
        &active_data_dir()?
            .join("User")
            .join("globalStorage"),
    );
    adopt_live_login_if_needed(&mut cfg)?;
    let captured = try_capture_pending(&mut cfg)?;
    for account in &mut cfg.accounts {
        refresh_account_identity_from_snapshot(account);
    }
    save_config(&cfg)?;
    Ok(ListStateResult {
        state: state_from(cfg)?,
        captured_email: captured.into_iter().next(),
        recovered,
    })
}

/// Pool folders (`Cursor.multi-cursor.<id>`, `.cursor.multi-cursor.<id>`)
/// whose id is not in the config hold real Cursor data whose config entry
/// was lost (config deleted or reset). Bring them back as environments
/// instead of trashing them; the user can delete them from the UI.
/// Returns the names of recovered environments.
fn recover_orphan_pool_dirs(cfg: &mut AppConfig) -> Result<Vec<String>, String> {
    let known: std::collections::HashSet<String> =
        cfg.environments.iter().map(|e| e.id.clone()).collect();
    let mut orphan_ids: std::collections::BTreeSet<String> = Default::default();
    for (dir, prefix) in [
        (application_support_dir()?, DATA_INACTIVE_PREFIX),
        (home_dir()?, DOT_INACTIVE_PREFIX),
    ] {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if let Some(id) = name.strip_prefix(prefix) {
                if !id.is_empty() && !known.contains(id) && entry.path().is_dir() {
                    orphan_ids.insert(id.to_string());
                }
            }
        }
    }

    let mut recovered = Vec::new();
    for id in orphan_ids {
        let short: String = id.trim_start_matches(['_']).chars().take(8).collect();
        let mut name = format!("Recovered environment ({short})");
        let mut n = 2;
        while cfg
            .environments
            .iter()
            .any(|e| e.name.eq_ignore_ascii_case(&name))
        {
            name = format!("Recovered environment ({short}) {n}");
            n += 1;
        }
        cfg.environments.push(Environment {
            id,
            name: name.clone(),
            created_at: now_iso(),
        });
        recovered.push(name);
    }
    Ok(recovered)
}

/// Disk usage (bytes) for an environment's data + ~/.cursor pool folders.
/// Runs `du` off the main thread so switching environments does not freeze the UI.
#[tauri::command]
pub async fn environment_disk_usage(env_id: String) -> Result<u64, String> {
    let cfg = load_config()?;
    if !cfg.environments.iter().any(|e| e.id == env_id) {
        return Err("Environment not found".to_string());
    }
    let active = cfg.active.env_id.as_deref();
    let data = env_data_dir(&env_id, active)?;
    let dot = env_dot_cursor_dir(&env_id, active)?;
    tauri::async_runtime::spawn_blocking(move || {
        Ok(dir_size_bytes(&data).saturating_add(dir_size_bytes(&dot)))
    })
    .await
    .map_err(|e| format!("Disk usage task failed: {e}"))?
}

/// Create an empty inactive environment and return immediately (no copy).
#[tauri::command]
pub fn create_environment(name: String) -> Result<AppState, String> {
    let name = name.trim().to_string();
    if name.is_empty() {
        return Err("Environment name is required".to_string());
    }
    let mut cfg = load_config()?;
    bootstrap_default_environment(&mut cfg)?;

    if cfg
        .environments
        .iter()
        .any(|e| e.name.eq_ignore_ascii_case(&name))
    {
        return Err(format!("Environment \"{name}\" already exists"));
    }

    let id = new_id();
    prepare_inactive_environment(&id)?;
    cfg.environments.push(Environment {
        id,
        name,
        created_at: now_iso(),
    });
    save_config(&cfg)?;
    state_from(cfg)
}

/// Copy the current (active) Cursor folders into an existing inactive environment.
#[tauri::command]
pub async fn copy_environment_from_current(
    app: AppHandle,
    env_id: String,
) -> Result<AppState, String> {
    let cfg = load_config()?;
    if !cfg.environments.iter().any(|e| e.id == env_id) {
        return Err("Environment not found".to_string());
    }
    if cfg.active.env_id.as_deref() == Some(env_id.as_str()) {
        return Err("Cannot copy into the current environment.".to_string());
    }
    if cursor_running() {
        return Err(
            "Quit Cursor before copying the current environment (data files may be locked)."
                .to_string(),
        );
    }

    let src_data = active_data_dir()?;
    let src_dot = active_dot_cursor_dir()?;
    let dst_data = inactive_data_dir(&env_id)?;
    let dst_dot = inactive_dot_cursor_dir(&env_id)?;
    let accounts = root_dir()?.join("accounts").join(&env_id);

    if !src_data.exists() {
        return Err(format!(
            "Nothing to copy: {} does not exist",
            src_data.display()
        ));
    }

    emit_progress(&app, 0, "Copying Cursor data…");

    let app_copy = app.clone();
    let copy_id = env_id.clone();
    let copy_result = tauri::async_runtime::spawn_blocking(move || {
        let result = (|| -> Result<(), String> {
            copy_cursor_tree_with_progress(
                &app_copy,
                &src_data,
                &dst_data,
                "Copying Cursor data…",
                0,
                50,
            )?;
            if src_dot.exists() {
                copy_cursor_tree_with_progress(
                    &app_copy,
                    &src_dot,
                    &dst_dot,
                    "Copying ~/.cursor…",
                    50,
                    100,
                )?;
            } else {
                std::fs::create_dir_all(&dst_dot).map_err(|e| e.to_string())?;
            }
            std::fs::create_dir_all(&accounts).map_err(|e| e.to_string())?;
            emit_progress(&app_copy, 100, "Copy complete");
            Ok(())
        })();

        if result.is_err() {
            cleanup_partial_copy(&dst_data);
            cleanup_partial_copy(&dst_dot);
            // Recreate empty skeletons so the env remains usable.
            let _ = prepare_inactive_environment(&copy_id);
        }
        result
    })
    .await
    .map_err(|e| format!("Copy task failed: {e}"))?;

    if let Err(err) = copy_result {
        emit_progress(&app, 0, "Copy failed");
        // Keep the environment listed — only the data copy failed.
        return Err(err);
    }

    let cfg = load_config()?;
    state_from(cfg)
}

#[tauri::command]
pub fn rename_environment(id: String, name: String) -> Result<AppState, String> {
    let name = name.trim().to_string();
    if name.is_empty() {
        return Err("Environment name is required".to_string());
    }
    let mut cfg = load_config()?;
    if cfg
        .environments
        .iter()
        .any(|e| e.id != id && e.name.eq_ignore_ascii_case(&name))
    {
        return Err(format!("Environment \"{name}\" already exists"));
    }
    let env = cfg
        .environments
        .iter_mut()
        .find(|e| e.id == id)
        .ok_or_else(|| "Environment not found".to_string())?;
    env.name = name;
    save_config(&cfg)?;
    state_from(cfg)
}

/// Set or clear (empty `label`) an account's custom name. Only Multi Cursor's
/// config changes, so Cursor may keep running.
#[tauri::command]
pub fn rename_account(id: String, label: String) -> Result<AppState, String> {
    let label = label.trim();
    let mut cfg = load_config()?;
    let account = cfg
        .accounts
        .iter_mut()
        .find(|a| a.id == id)
        .ok_or_else(|| "Account not found".to_string())?;
    account.label = (!label.is_empty()).then(|| label.to_string());
    save_config(&cfg)?;
    state_from(cfg)
}

#[tauri::command]
pub async fn delete_environment(app: AppHandle, id: String) -> Result<AppState, String> {
    let mut cfg = load_config()?;
    if !cfg.environments.iter().any(|e| e.id == id) {
        return Err("Environment not found".to_string());
    }
    if cfg.active.env_id.as_deref() == Some(id.as_str()) {
        return Err(
            "Cannot delete the current environment. Switch to another environment first."
                .to_string(),
        );
    }
    if cfg.environments.len() <= 1 {
        return Err("Cannot delete the only environment.".to_string());
    }

    // Inactive pool folders only — never touch the live Cursor / ~/.cursor names.
    let paths = vec![
        (
            inactive_data_dir(&id)?,
            format!("Moving Cursor data to {}…", crate::platform::trash_name()),
        ),
        (
            inactive_dot_cursor_dir(&id)?,
            format!("Moving ~/.cursor to {}…", crate::platform::trash_name()),
        ),
        (
            root_dir()?.join("accounts").join(&id),
            "Removing saved logins…".to_string(),
        ),
    ];

    emit_progress(&app, 0, "Preparing…");
    let app_trash = app.clone();
    tauri::async_runtime::spawn_blocking(move || trash_paths_with_progress(&app_trash, &paths))
        .await
        .map_err(|e| format!("Delete task failed: {e}"))??;

    cfg.environments.retain(|e| e.id != id);
    cfg.accounts.retain(|a| a.env_id != id);
    save_config(&cfg)?;
    emit_progress(
        &app,
        100,
        format!("Moved to {}", crate::platform::trash_name()),
    );
    state_from(cfg)
}

#[tauri::command]
pub fn create_account(env_id: String) -> Result<AppState, String> {
    if cursor_running() {
        return Err(
            "Cursor is still running. Wait for it to quit before adding an account.".to_string(),
        );
    }

    let mut cfg = load_config()?;
    bootstrap_default_environment(&mut cfg)?;
    if !cfg.environments.iter().any(|e| e.id == env_id) {
        return Err("Environment not found".to_string());
    }

    discard_abandoned_pending(&mut cfg, None)?;
    switch_environment(&mut cfg, &env_id)?;
    save_active_account_tokens(&cfg, &env_id)?;

    let id = new_id();
    clear_auth_keys(&state_db(&cfg, &env_id)?)?;
    save_snapshot(&env_id, &id, &AuthSnapshot::default())?;

    cfg.accounts.push(Account {
        id: id.clone(),
        env_id: env_id.clone(),
        name: "Signing in…".to_string(),
        email: None,
        updated_at: now_iso(),
        pending_login: true,
        label: None,
    });
    cfg.active.env_id = Some(env_id);
    cfg.active.account_id = Some(id);
    save_config(&cfg)?;

    launch_cursor(&cfg.cursor_launch())?;
    state_from(cfg)
}

#[tauri::command]
pub fn delete_account(id: String) -> Result<AppState, String> {
    let mut cfg = load_config()?;
    let account = cfg
        .accounts
        .iter()
        .find(|a| a.id == id)
        .cloned()
        .ok_or_else(|| "Account not found".to_string())?;

    let is_active = cfg.active.account_id.as_deref() == Some(id.as_str());
    // Only the live auth DB needs Cursor quit. Inactive accounts are just a
    // Multi Cursor snapshot + config entry.
    if is_active && cursor_running() {
        return Err(
            "Cursor is still running. Wait for it to quit before deleting the current account."
                .to_string(),
        );
    }

    delete_snapshot(&account.env_id, &account.id)?;
    cfg.accounts.retain(|a| a.id != id);
    crate::usage::forget(&id);

    if is_active {
        cfg.active.account_id = cfg
            .accounts
            .iter()
            .find(|a| a.env_id == account.env_id)
            .map(|a| a.id.clone());
        if let Some(next_id) = cfg.active.account_id.clone() {
            let snap = load_snapshot(&account.env_id, &next_id)?;
            write_auth_keys(&state_db(&cfg, &account.env_id)?, &snap)?;
        } else {
            clear_auth_keys(&state_db(&cfg, &account.env_id)?)?;
        }
    }

    save_config(&cfg)?;
    state_from(cfg)
}

/// Make `env_id` / `account_id` Cursor's current setup, then start Cursor
/// unless `open` is false ("Switch": the user opens Cursor themselves).
#[tauri::command]
pub fn launch(
    env_id: String,
    account_id: Option<String>,
    open: Option<bool>,
) -> Result<AppState, String> {
    let mut cfg = load_config()?;
    bootstrap_default_environment(&mut cfg)?;
    if !cfg.environments.iter().any(|e| e.id == env_id) {
        return Err("Environment not found".to_string());
    }

    // Cursor holds the auth database while it runs.
    if cursor_running() {
        let same = account_id.is_some()
            && cfg.active.env_id.as_deref() == Some(env_id.as_str())
            && cfg.active.account_id == account_id;
        return Err(if same {
            "Cursor is already running with this account.".to_string()
        } else {
            "Cursor is still running. Wait for it to quit before switching.".to_string()
        });
    }

    if let Some(ref account_id) = account_id {
        apply_account(&mut cfg, &env_id, account_id)?;
    } else {
        discard_abandoned_pending(&mut cfg, None)?;
        switch_environment(&mut cfg, &env_id)?;
        cfg.active.account_id = None;
        save_config(&cfg)?;
    }

    if open.unwrap_or(true) {
        launch_cursor(&cfg.cursor_launch())?;
    }
    state_from(cfg)
}

/// Ask Cursor to quit without waiting — UI polls `is_cursor_running` afterward.
#[tauri::command]
pub fn quit_cursor_cmd() -> Result<(), String> {
    request_quit_cursor()
}

#[tauri::command]
pub fn force_quit_cursor_cmd() -> Result<(), String> {
    force_quit_cursor()
}

#[tauri::command]
pub fn is_cursor_running() -> bool {
    cursor_running()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{read_auth_keys, write_auth_keys, AuthSnapshot};
    use crate::paths::test_sandbox;

    fn login(email: &str) -> AuthSnapshot {
        let mut snap = AuthSnapshot::default();
        snap.keys.insert("cursorAuth/accessToken".into(), format!("token-{email}"));
        snap.keys.insert("cursorAuth/cachedEmail".into(), email.into());
        snap
    }

    /// One environment with signed-in account A active.
    fn setup() -> (AppConfig, String, String) {
        let mut cfg = AppConfig::default();
        bootstrap_default_environment(&mut cfg).unwrap();
        let env = cfg.active.env_id.clone().unwrap();
        let a = new_id();
        save_snapshot(&env, &a, &login("a@example.com")).unwrap();
        write_auth_keys(&state_db(&cfg, &env).unwrap(), &login("a@example.com")).unwrap();
        cfg.accounts.push(Account {
            id: a.clone(),
            env_id: env.clone(),
            name: "a@example.com".into(),
            email: Some("a@example.com".into()),
            updated_at: now_iso(),
            pending_login: false,
            label: None,
        });
        cfg.active.account_id = Some(a.clone());
        (cfg, env, a)
    }

    /// create_account without launching Cursor.
    fn start_new_account(cfg: &mut AppConfig, env: &str) -> String {
        discard_abandoned_pending(cfg, None).unwrap();
        save_active_account_tokens(cfg, env).unwrap();
        let id = new_id();
        clear_auth_keys(&state_db(cfg, env).unwrap()).unwrap();
        save_snapshot(env, &id, &AuthSnapshot::default()).unwrap();
        cfg.accounts.push(Account {
            id: id.clone(),
            env_id: env.into(),
            name: "Signing in…".into(),
            email: None,
            updated_at: now_iso(),
            pending_login: true,
            label: None,
        });
        cfg.active.account_id = Some(id.clone());
        id
    }

    fn emails(cfg: &AppConfig) -> Vec<String> {
        let mut v: Vec<String> = cfg
            .accounts
            .iter()
            .map(|a| a.email.clone().unwrap_or_else(|| a.name.clone()))
            .collect();
        v.sort();
        v
    }

    #[test]
    fn abandoned_sign_in_does_not_duplicate_other_account() {
        let (_g, root) = test_sandbox();
        let (mut cfg, env, a) = setup();
        let _pending = start_new_account(&mut cfg, &env);
        // Login window closed without signing in; user launches account A.
        apply_account(&mut cfg, &env, &a).unwrap();
        // UI poll afterwards.
        try_capture_pending(&mut cfg).unwrap();
        assert_eq!(emails(&cfg), vec!["a@example.com"]);
        assert_eq!(cfg.active.account_id.as_deref(), Some(a.as_str()));
        let live = read_auth_keys(&state_db(&cfg, &env).unwrap()).unwrap();
        assert_eq!(email_from_snapshot(&live).as_deref(), Some("a@example.com"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn signing_in_as_existing_account_updates_it() {
        let (_g, root) = test_sandbox();
        let (mut cfg, env, a) = setup();
        start_new_account(&mut cfg, &env);
        write_auth_keys(&state_db(&cfg, &env).unwrap(), &login("A@example.com")).unwrap();
        try_capture_pending(&mut cfg).unwrap();
        assert_eq!(emails(&cfg), vec!["A@example.com"]);
        assert_eq!(cfg.active.account_id.as_deref(), Some(a.as_str()));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn sign_in_before_poll_is_kept_when_switching() {
        let (_g, root) = test_sandbox();
        let (mut cfg, env, a) = setup();
        start_new_account(&mut cfg, &env);
        // Signed in as B, but launched A before the UI polled.
        write_auth_keys(&state_db(&cfg, &env).unwrap(), &login("b@example.com")).unwrap();
        apply_account(&mut cfg, &env, &a).unwrap();
        assert_eq!(emails(&cfg), vec!["a@example.com", "b@example.com"]);
        let b = cfg.accounts.iter().find(|x| x.email.as_deref() == Some("b@example.com")).unwrap();
        assert!(!b.pending_login);
        let b_snap = crate::auth::load_snapshot(&env, &b.id).unwrap();
        assert_eq!(email_from_snapshot(&b_snap).as_deref(), Some("b@example.com"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn custom_name_survives_refresh_and_merge() {
        let (_g, root) = test_sandbox();
        let (mut cfg, env, a) = setup();
        save_config(&cfg).unwrap();

        // Rename A; the automatic refresh must not overwrite the custom name.
        let state = rename_account(a.clone(), "  Work  ".into()).unwrap();
        let acct = state.config.accounts.iter().find(|x| x.id == a).unwrap();
        assert_eq!(acct.label.as_deref(), Some("Work"));
        let mut acct = acct.clone();
        refresh_account_identity_from_snapshot(&mut acct);
        assert_eq!(acct.label.as_deref(), Some("Work"));
        assert_eq!(acct.name, "a@example.com");

        // Empty clears it.
        let state = rename_account(a.clone(), " ".into()).unwrap();
        assert!(state.config.accounts.iter().find(|x| x.id == a).unwrap().label.is_none());

        // A name given while signing in carries over when the sign-in turns
        // out to be an existing account.
        cfg = load_config().unwrap();
        let pending = start_new_account(&mut cfg, &env);
        cfg.accounts.iter_mut().find(|x| x.id == pending).unwrap().label = Some("Main".into());
        write_auth_keys(&state_db(&cfg, &env).unwrap(), &login("a@example.com")).unwrap();
        try_capture_pending(&mut cfg).unwrap();
        assert_eq!(cfg.accounts.len(), 1);
        assert_eq!(cfg.accounts[0].label.as_deref(), Some("Main"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn launching_pending_account_right_after_sign_in_keeps_the_login() {
        let (_g, root) = test_sandbox();
        let (mut cfg, env, _a) = setup();
        let pending = start_new_account(&mut cfg, &env);
        // Signed in as B; user launches the pending account before any poll.
        write_auth_keys(&state_db(&cfg, &env).unwrap(), &login("b@example.com")).unwrap();
        apply_account(&mut cfg, &env, &pending).unwrap();
        let acct = cfg.accounts.iter().find(|x| x.id == pending).unwrap();
        assert!(!acct.pending_login);
        assert_eq!(cfg.active.account_id.as_deref(), Some(pending.as_str()));
        let live = read_auth_keys(&state_db(&cfg, &env).unwrap()).unwrap();
        assert_eq!(email_from_snapshot(&live).as_deref(), Some("b@example.com"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn launching_pending_account_that_merges_uses_the_existing_account() {
        let (_g, root) = test_sandbox();
        let (mut cfg, env, a) = setup();
        let pending = start_new_account(&mut cfg, &env);
        // Signed in again as A; launching the pending entry must end on A.
        write_auth_keys(&state_db(&cfg, &env).unwrap(), &login("a@example.com")).unwrap();
        apply_account(&mut cfg, &env, &pending).unwrap();
        assert_eq!(emails(&cfg), vec!["a@example.com"]);
        assert_eq!(cfg.active.account_id.as_deref(), Some(a.as_str()));
        let live = read_auth_keys(&state_db(&cfg, &env).unwrap()).unwrap();
        assert_eq!(email_from_snapshot(&live).as_deref(), Some("a@example.com"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn lost_config_recovers_environments_instead_of_trashing_them() {
        let (_g, root) = test_sandbox();
        // An inactive environment with data, plus a half-present one.
        prepare_inactive_environment("1234abcd-old").unwrap();
        std::fs::write(inactive_data_dir("1234abcd-old").unwrap().join("marker"), "keep").unwrap();
        std::fs::create_dir_all(inactive_dot_cursor_dir("5678ef90-half").unwrap()).unwrap();
        // No config.json: first start after the config was lost.
        assert!(!crate::paths::config_path().unwrap().exists());

        let result = list_state().unwrap();
        let names: Vec<&str> = result.state.config.environments.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"Default"));
        assert!(names.contains(&"Recovered environment (1234abcd)"));
        assert!(names.contains(&"Recovered environment (5678ef90)"));
        assert_eq!(result.recovered.len(), 2);
        // Data is still there, and the half-present one got its missing folder.
        let marker = inactive_data_dir("1234abcd-old").unwrap().join("marker");
        assert_eq!(std::fs::read_to_string(marker).unwrap(), "keep");
        assert!(inactive_data_dir("5678ef90-half").unwrap().exists());

        // Next refresh: nothing new to recover, nothing trashed.
        let again = list_state().unwrap();
        assert!(again.recovered.is_empty());
        assert_eq!(again.state.config.environments.len(), 3);
        // Atomic save leaves no temp file behind.
        assert!(!crate::paths::config_path().unwrap().with_extension("json.tmp").exists());
        let _ = std::fs::remove_dir_all(root);
    }
}
