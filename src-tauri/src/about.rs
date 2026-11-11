//! Native About panel content for Multi Cursor.

pub const APP_NAME: &str = "Multi Cursor";
pub const REPO_URL: &str = "https://github.com/claudio-silva/multi-cursor";

const DESCRIPTION: &str = "Switch between Cursor accounts and isolated environments.";
const COPYRIGHT: &str = "© 2026 Cláudio Silva";

/// Credits text including the repository URL (used where attributed links are unavailable).
#[cfg(target_os = "macos")]
pub fn credits_text() -> String {
    format!("\n{DESCRIPTION}\n\n{COPYRIGHT}\n{REPO_URL}")
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AboutInfo {
    name: &'static str,
    version: String,
    description: &'static str,
    copyright: &'static str,
    repo_url: &'static str,
}

/// About details for the in-window About dialog (Linux / Windows have no app menu).
#[tauri::command]
pub fn about_info(app: tauri::AppHandle) -> AboutInfo {
    AboutInfo {
        name: APP_NAME,
        version: app.package_info().version.to_string(),
        description: DESCRIPTION,
        copyright: COPYRIGHT,
        repo_url: REPO_URL,
    }
}

/// Show the macOS standard About panel with a clickable repository link in credits.
#[cfg(target_os = "macos")]
pub fn show(version: &str) {
    use objc2::runtime::AnyObject;
    use objc2::AnyThread;
    use objc2_app_kit::{
        NSAboutPanelOptionApplicationName, NSAboutPanelOptionApplicationVersion,
        NSAboutPanelOptionCredits, NSAboutPanelOptionVersion, NSApplication, NSLinkAttributeName,
    };
    use objc2_foundation::{
        MainThreadMarker, NSDictionary, NSMutableAttributedString, NSRange, NSString, NSURL,
    };

    let Some(mtm) = MainThreadMarker::new() else {
        eprintln!("about panel: not on main thread");
        return;
    };

    let credits_body = credits_text();
    let credits_ns = NSString::from_str(&credits_body);
    let credits = NSMutableAttributedString::initWithString(
        NSMutableAttributedString::alloc(),
        &credits_ns,
    );

    let url_ns = NSString::from_str(REPO_URL);
    if let Some(url) = NSURL::URLWithString(&url_ns) {
        let total_len = credits_ns.length();
        let url_len = url_ns.length();
        if url_len > 0 && url_len <= total_len {
            let range = NSRange::new((total_len - url_len) as usize, url_len as usize);
            unsafe {
                credits.addAttribute_value_range(
                    NSLinkAttributeName,
                    &*(url.as_ref() as *const NSURL as *const AnyObject),
                    range,
                );
            }
        }
    }

    let name = NSString::from_str(APP_NAME);
    let version_ns = NSString::from_str(version);
    // Empty build version suppresses the "(x.y.z)" that macOS otherwise pulls from CFBundleVersion.
    let build_ns = NSString::from_str("");

    let keys = [
        unsafe { NSAboutPanelOptionApplicationName },
        unsafe { NSAboutPanelOptionApplicationVersion },
        unsafe { NSAboutPanelOptionVersion },
        unsafe { NSAboutPanelOptionCredits },
    ];
    let objects: [objc2::rc::Retained<AnyObject>; 4] = [
        objc2::rc::Retained::into_super(objc2::rc::Retained::into_super(name)),
        objc2::rc::Retained::into_super(objc2::rc::Retained::into_super(version_ns)),
        objc2::rc::Retained::into_super(objc2::rc::Retained::into_super(build_ns)),
        objc2::rc::Retained::into_super(objc2::rc::Retained::into_super(
            objc2::rc::Retained::into_super(credits),
        )),
    ];
    let dict = NSDictionary::from_retained_objects(&keys, &objects);

    unsafe {
        NSApplication::sharedApplication(mtm).orderFrontStandardAboutPanelWithOptions(&dict);
    }
}
