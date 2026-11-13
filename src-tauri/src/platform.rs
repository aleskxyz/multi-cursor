//! OS-specific locations and process names.
//!
//! Everything that differs between macOS, Linux, and Windows about *where*
//! Cursor keeps its data and *what* its process is called lives here.

use std::path::PathBuf;

/// Name of the folder Cursor uses for its user data on every platform.
pub const CURSOR_DATA_DIR_NAME: &str = "Cursor";

/// Test-only root that replaces the home and data folders on every OS.
/// (Environment variables can't do this: on Windows the `dirs` crate reads
/// the real known folders and ignores them.)
#[cfg(test)]
pub(crate) static TEST_ROOT: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

#[cfg(test)]
fn test_root() -> Option<PathBuf> {
    TEST_ROOT.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// The current user's home folder (`$HOME` / `%USERPROFILE%`).
pub fn home_dir() -> Result<PathBuf, String> {
    #[cfg(test)]
    if let Some(root) = test_root() {
        return Ok(root);
    }
    dirs::home_dir().ok_or_else(|| "Could not determine the home folder".to_string())
}

/// Parent of Cursor's user-data folder.
///
/// - macOS: `~/Library/Application Support`
/// - Linux: `$XDG_CONFIG_HOME` or `~/.config`
/// - Windows: `%APPDATA%` (Roaming)
pub fn cursor_data_parent() -> Result<PathBuf, String> {
    #[cfg(test)]
    if let Some(root) = test_root() {
        return Ok(root.join(".config"));
    }
    #[cfg(target_os = "macos")]
    {
        Ok(home_dir()?.join("Library").join("Application Support"))
    }
    #[cfg(not(target_os = "macos"))]
    {
        dirs::config_dir().ok_or_else(|| "Could not determine the config folder".to_string())
    }
}

/// The macOS default that older configs stored verbatim.
pub const LEGACY_MACOS_APP_PATH: &str = "/Applications/Cursor.app";

/// How to start Cursor: a program plus any arguments a desktop entry asked for
/// (e.g. `--no-sandbox` for AppImages on Ubuntu 24.04+).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorLaunch {
    pub program: String,
    pub args: Vec<String>,
}

impl CursorLaunch {
    pub fn program(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
        }
    }
}

/// Best guess for how to start Cursor on this machine. Returns the first
/// candidate that exists, or the platform's primary default when none does.
pub fn default_cursor_launch() -> CursorLaunch {
    #[cfg(target_os = "macos")]
    {
        CursorLaunch::program(LEGACY_MACOS_APP_PATH)
    }

    #[cfg(target_os = "windows")]
    {
        let mut candidates: Vec<PathBuf> = Vec::new();
        if let Some(local) = dirs::data_local_dir() {
            candidates.push(local.join("Programs").join("cursor").join("Cursor.exe"));
        }
        if let Some(pf) = std::env::var_os("ProgramFiles") {
            candidates.push(PathBuf::from(pf).join("Cursor").join("Cursor.exe"));
        }
        let path = first_existing(&candidates)
            .or_else(|| candidates.first().cloned())
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "Cursor.exe".to_string());
        CursorLaunch::program(path)
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        linux_find_cursor().unwrap_or_else(|| CursorLaunch::program("/usr/share/cursor/cursor"))
    }
}

/// Find a Cursor install on Linux, in order of preference:
/// 1. The official .deb / .rpm location (`/usr/share/cursor/cursor`).
/// 2. Desktop entries named "Cursor" — this is how AppImage integrators
///    (AppImageLauncher, Gear Lever) and manual setups register an AppImage,
///    wherever the file lives. Their arguments are kept.
/// 3. `Cursor*.AppImage` in common folders (newest file wins).
/// 4. `cursor` on PATH.
#[cfg(all(unix, not(target_os = "macos")))]
fn linux_find_cursor() -> Option<CursorLaunch> {
    let fixed = [
        PathBuf::from("/usr/share/cursor/cursor"),
        PathBuf::from("/opt/Cursor/cursor"),
        PathBuf::from("/opt/cursor/cursor"),
    ];
    if let Some(p) = first_existing(&fixed) {
        return Some(CursorLaunch::program(p.display().to_string()));
    }
    if let Some(launch) = cursor_from_desktop_entries() {
        return Some(launch);
    }
    if let Ok(home) = home_dir() {
        let dirs = [
            home.join("Applications"),
            home.join("applications"),
            home.join("AppImages"),
            home.join("Apps"),
            home.join(".local").join("bin"),
            home.join("bin"),
            home.join("Downloads"),
            PathBuf::from("/opt"),
        ];
        let mut found: Vec<PathBuf> = dirs.iter().flat_map(|d| find_appimages(d)).collect();
        found.sort_by_key(|p| std::cmp::Reverse(modified(p)));
        if let Some(p) = found.into_iter().next() {
            return Some(CursorLaunch::program(p.display().to_string()));
        }
    }
    which("cursor").map(|p| CursorLaunch::program(p.display().to_string()))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn modified(p: &std::path::Path) -> std::time::SystemTime {
    std::fs::metadata(p)
        .and_then(|m| m.modified())
        .unwrap_or(std::time::UNIX_EPOCH)
}

/// Launch command from the first `[Desktop Entry]` with `Name=Cursor` whose
/// program exists. User entries (`~/.local/share/applications`) win over
/// system ones, matching how desktops resolve them.
#[cfg(all(unix, not(target_os = "macos")))]
fn cursor_from_desktop_entries() -> Option<CursorLaunch> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(data) = dirs::data_dir() {
        dirs.push(data.join("applications"));
    }
    let system = std::env::var("XDG_DATA_DIRS")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".to_string());
    dirs.extend(
        system
            .split(':')
            .map(|d| PathBuf::from(d).join("applications")),
    );

    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut files: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "desktop"))
            .collect();
        files.sort();
        for file in files {
            let Ok(text) = std::fs::read_to_string(&file) else {
                continue;
            };
            if let Some(mut launch) = parse_cursor_desktop_entry(&text) {
                if !launch.program.starts_with('/') {
                    match which(&launch.program) {
                        Some(p) => launch.program = p.display().to_string(),
                        None => continue,
                    }
                }
                if std::path::Path::new(&launch.program).is_file() {
                    return Some(launch);
                }
            }
        }
    }
    None
}

/// Launch command from the main `[Desktop Entry]` group when it has
/// `Name=Cursor` exactly (so "Multi Cursor" and URL handlers don't match).
/// Flatpak / Snap entries are skipped: those keep Cursor's data in their own
/// sandbox folders, which Multi Cursor doesn't manage.
#[cfg(all(unix, not(target_os = "macos")))]
fn parse_cursor_desktop_entry(text: &str) -> Option<CursorLaunch> {
    let mut in_main = false;
    let mut name = None;
    let mut exec = None;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_main = line == "[Desktop Entry]";
            continue;
        }
        if !in_main {
            continue;
        }
        if let Some(v) = line.strip_prefix("Name=") {
            name = Some(v.trim());
        } else if let Some(v) = line.strip_prefix("Exec=") {
            exec = Some(v.trim());
        }
    }
    if name != Some("Cursor") {
        return None;
    }
    let launch = parse_desktop_exec(exec?)?;
    let base = launch.program.rsplit('/').next().unwrap_or("");
    let sandboxed = matches!(base, "flatpak" | "snap")
        || launch.program.starts_with("/snap/")
        || launch.program.starts_with("/var/lib/snapd/")
        || launch.program.starts_with("/var/lib/flatpak/");
    (!sandboxed).then_some(launch)
}

/// Split an `Exec=` value into program and arguments per the Desktop Entry
/// spec: double-quoted words (with `\"`, `\\`, `` \` ``, `\$` escapes), field
/// codes like `%U` removed, `%%` → `%`. A leading `env [-u NAME] [VAR=x]…`
/// wrapper is unwrapped (its variables are not applied).
#[cfg(all(unix, not(target_os = "macos")))]
fn parse_desktop_exec(exec: &str) -> Option<CursorLaunch> {
    let mut words: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut quoted = false;
    let mut chars = exec.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                quoted = !quoted;
                in_word = true;
            }
            '\\' if quoted => {
                if let Some(&n) = chars.peek() {
                    if matches!(n, '"' | '\\' | '`' | '$') {
                        cur.push(n);
                        chars.next();
                        continue;
                    }
                }
                cur.push('\\');
            }
            c if c.is_whitespace() && !quoted => {
                if in_word {
                    words.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            c => {
                cur.push(c);
                in_word = true;
            }
        }
    }
    if in_word {
        words.push(cur);
    }

    // Drop field codes (%f %F %u %U %i %c %k and deprecated ones); %% → %.
    let words: Vec<String> = words
        .into_iter()
        .filter(|w| !(w.len() == 2 && w.starts_with('%') && w != "%%"))
        .map(|w| w.replace("%%", "%"))
        .collect();

    let mut it = words.into_iter().peekable();
    let mut program = it.next()?;
    if program == "env" || program.ends_with("/env") {
        loop {
            let w = it.next()?;
            if w == "-u" || w == "--unset" {
                it.next()?;
            } else if w.starts_with('-') || w.contains('=') {
                continue;
            } else {
                program = w;
                break;
            }
        }
    }
    Some(CursorLaunch {
        program,
        args: it.collect(),
    })
}

#[cfg(not(target_os = "macos"))]
fn first_existing(candidates: &[PathBuf]) -> Option<PathBuf> {
    candidates.iter().find(|p| p.is_file()).cloned()
}

#[cfg(all(unix, not(target_os = "macos")))]
fn find_appimages(dir: &std::path::Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| {
                    let lower = n.to_ascii_lowercase();
                    lower.starts_with("cursor") && lower.ends_with(".appimage")
                })
                .unwrap_or(false)
        })
        .collect();
    found.retain(|p| p.is_file());
    found
}

#[cfg(all(unix, not(target_os = "macos")))]
fn which(bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(bin))
        .find(|p| p.is_file())
        // `cursor` on PATH is often a symlink to the real binary or a wrapper
        // script; either launches Cursor with default paths.
        .map(|p| std::fs::canonicalize(&p).unwrap_or(p))
}

/// True when `name` (a process name) / `exe` (its executable path) look like
/// the Cursor IDE *main* process rather than a helper or Multi Cursor itself.
#[cfg(not(target_os = "macos"))]
pub fn is_cursor_main_process(name: &str, exe: Option<&std::path::Path>) -> bool {
    #[cfg(target_os = "windows")]
    {
        let _ = exe;
        name.eq_ignore_ascii_case("Cursor.exe") || name.eq_ignore_ascii_case("Cursor")
    }
    #[cfg(not(target_os = "windows"))]
    {
        if name == "cursor" || name == "Cursor" {
            return true;
        }
        // AppImages run from a mount like /tmp/.mount_CursorXXXX/cursor; the
        // process name can be truncated, so also check the exe file name.
        exe.and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .map(|n| n == "cursor" || n == "Cursor")
            .unwrap_or(false)
    }
}

/// What the OS calls the place deleted files go.
pub fn trash_name() -> &'static str {
    #[cfg(target_os = "windows")]
    {
        "Recycle Bin"
    }
    #[cfg(not(target_os = "windows"))]
    {
        "Trash"
    }
}

/// Short OS identifier for the frontend: `macos`, `linux`, or `windows`.
pub fn os_id() -> &'static str {
    std::env::consts::OS
}

#[cfg(all(test, unix, not(target_os = "macos")))]
mod tests {
    use super::*;

    fn launch(program: &str, args: &[&str]) -> Option<CursorLaunch> {
        Some(CursorLaunch {
            program: program.into(),
            args: args.iter().map(|a| a.to_string()).collect(),
        })
    }

    #[test]
    fn official_deb_desktop_entry() {
        // From cursor_3.29.15_amd64.deb (trimmed).
        let text = "[Desktop Entry]\nName=Cursor\nExec=/usr/share/cursor/cursor %F\nStartupWMClass=Cursor\n\n[Desktop Action new-empty-window]\nName=New Empty Window\nExec=/usr/share/cursor/cursor --new-window %F\n";
        assert_eq!(
            parse_cursor_desktop_entry(text),
            launch("/usr/share/cursor/cursor", &[])
        );
    }

    #[test]
    fn ignores_multi_cursor_and_url_handler() {
        let ours = "[Desktop Entry]\nName=Multi Cursor\nExec=multi-cursor\n";
        assert_eq!(parse_cursor_desktop_entry(ours), None);
        let url = "[Desktop Entry]\nName=Cursor - URL Handler\nExec=/usr/share/cursor/cursor --open-url %U\n";
        assert_eq!(parse_cursor_desktop_entry(url), None);
    }

    #[test]
    fn appimage_integrator_entries_keep_arguments() {
        // Quoted path with spaces; --no-sandbox must survive, %U must not.
        let quoted = "[Desktop Entry]\nName=Cursor\nExec=\"/home/me/My Apps/Cursor-3.29.15-x86_64.AppImage\" --no-sandbox %U\n";
        assert_eq!(
            parse_cursor_desktop_entry(quoted),
            launch(
                "/home/me/My Apps/Cursor-3.29.15-x86_64.AppImage",
                &["--no-sandbox"]
            )
        );
        let env = "[Desktop Entry]\nName=Cursor\nExec=env -u FOO APPIMAGELAUNCHER_DISABLE=1 \"/home/me/Apps/cursor.AppImage\" --no-sandbox %U\n";
        assert_eq!(
            parse_cursor_desktop_entry(env),
            launch("/home/me/Apps/cursor.AppImage", &["--no-sandbox"])
        );
    }

    #[test]
    fn exec_quoting_and_percent_escape() {
        assert_eq!(
            parse_desktop_exec(r#""/opt/a \"b\"/cursor" --flag=100%% %f"#),
            launch(r#"/opt/a "b"/cursor"#, &["--flag=100%"])
        );
    }

    #[test]
    fn skips_flatpak_and_snap() {
        let flatpak = "[Desktop Entry]\nName=Cursor\nExec=/usr/bin/flatpak run --branch=stable com.cursor.Cursor %U\n";
        assert_eq!(parse_cursor_desktop_entry(flatpak), None);
        let snap = "[Desktop Entry]\nName=Cursor\nExec=/snap/bin/cursor %U\n";
        assert_eq!(parse_cursor_desktop_entry(snap), None);
    }
}
