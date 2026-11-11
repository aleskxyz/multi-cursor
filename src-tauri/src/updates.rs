//! Check GitHub Releases for a newer Multi Cursor version.

use serde::Serialize;
use tauri::AppHandle;

use crate::about::{REPO_SLUG, REPO_URL};

fn latest_release_api() -> String {
    format!("https://api.github.com/repos/{REPO_SLUG}/releases/latest")
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateCheckResult {
    pub update_available: bool,
    pub current_version: String,
    pub latest_version: String,
    pub release_url: String,
    pub message: String,
}

#[derive(Debug, serde::Deserialize)]
struct GithubRelease {
    tag_name: String,
    html_url: String,
}

fn normalize_version(raw: &str) -> String {
    raw.trim().trim_start_matches(['v', 'V']).to_string()
}

/// `(major, minor, patch, is_prerelease)`; build metadata (`+…`) is ignored.
fn parse_semver(raw: &str) -> Option<(u64, u64, u64, bool)> {
    let s = normalize_version(raw);
    let s = s.split('+').next().unwrap_or("");
    let (core, pre) = match s.split_once('-') {
        Some((core, pre)) => (core, !pre.is_empty()),
        None => (s, false),
    };
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().unwrap_or("0").parse().ok()?;
    let patch = parts.next().unwrap_or("0").parse().ok()?;
    Some((major, minor, patch, pre))
}

/// True when `latest` is a newer version than `current`. A release beats a
/// pre-release of the same version (1.2.0 > 1.2.0-beta.1).
fn is_newer(latest: &str, current: &str) -> Result<bool, String> {
    let (l1, l2, l3, l_pre) = parse_semver(latest)
        .ok_or_else(|| format!("Could not parse latest version \"{latest}\""))?;
    let (c1, c2, c3, c_pre) = parse_semver(current)
        .ok_or_else(|| format!("Could not parse current version \"{current}\""))?;
    Ok(match (l1, l2, l3).cmp(&(c1, c2, c3)) {
        std::cmp::Ordering::Greater => true,
        std::cmp::Ordering::Less => false,
        std::cmp::Ordering::Equal => c_pre && !l_pre,
    })
}

#[cfg(test)]
mod tests {
    use super::is_newer;

    #[test]
    fn version_comparison() {
        assert!(is_newer("v1.2.0", "1.1.9").unwrap());
        assert!(!is_newer("1.2.0", "1.2.0").unwrap());
        assert!(!is_newer("1.1.0", "1.2.0").unwrap());
        assert!(is_newer("1.2.0", "1.2.0-beta.1").unwrap());
        assert!(!is_newer("1.2.0-beta.2", "1.2.0").unwrap());
        assert!(is_newer("1.10.0", "1.9.0").unwrap());
        assert!(!is_newer("1.2.0+build.5", "1.2.0").unwrap());
    }
}

#[tauri::command]
pub async fn check_for_updates(app: AppHandle) -> Result<UpdateCheckResult, String> {
    let current_version = app.package_info().version.to_string();

    let client = reqwest::Client::builder()
        .user_agent(format!("multi-cursor/{current_version}"))
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| format!("Failed to create HTTP client: {e}"))?;

    let response = client
        .get(latest_release_api())
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| format!("Failed to reach GitHub: {e}"))?;

    let status = response.status();
    if status.as_u16() == 404 {
        return Ok(UpdateCheckResult {
            update_available: false,
            current_version: current_version.clone(),
            latest_version: current_version.clone(),
            release_url: format!("{REPO_URL}/releases"),
            message: format!("You're up to date (version {current_version})."),
        });
    }
    if !status.is_success() {
        return Err(format!("GitHub returned HTTP {status}"));
    }

    let release: GithubRelease = response
        .json()
        .await
        .map_err(|e| format!("Invalid GitHub response: {e}"))?;

    let latest_version = normalize_version(&release.tag_name);
    let update_available = is_newer(&latest_version, &current_version)?;

    let message = if update_available {
        format!("Version {latest_version} is available.")
    } else {
        format!("You're up to date (version {current_version}).")
    };

    Ok(UpdateCheckResult {
        update_available,
        current_version,
        latest_version,
        release_url: release.html_url,
        message,
    })
}

#[tauri::command]
pub fn open_url(url: String) -> Result<(), String> {
    // Opens in the default browser on every platform.
    let url = url.trim();
    if url.is_empty() {
        return Err("URL is required".to_string());
    }
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err("Only http(s) URLs can be opened".to_string());
    }
    tauri_plugin_opener::open_url(url, None::<&str>).map_err(|e| format!("Failed to open URL: {e}"))
}
