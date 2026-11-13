mod about;
mod auth;
mod commands;
mod copy_progress;
mod cursor;
mod paths;
mod platform;
mod safe_storage;
mod updates;
mod usage;
mod window_frame;

#[cfg(target_os = "macos")]
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem, Submenu};
#[cfg(target_os = "macos")]
use tauri::{AppHandle, Emitter};

/// macOS app menu. Linux and Windows get no menu bar (it would sit inside this
/// small window); About and Check for Updates live in the window footer there.
#[cfg(target_os = "macos")]
fn build_menu(app: &AppHandle) -> tauri::Result<Menu<tauri::Wry>> {
    let window_menu = Submenu::with_id_and_items(
        app,
        "window",
        "Window",
        true,
        &[
            &PredefinedMenuItem::minimize(app, None)?,
            &PredefinedMenuItem::maximize(app, None)?,
            &PredefinedMenuItem::separator(app)?,
            &PredefinedMenuItem::close_window(app, None)?,
        ],
    )?;

    let about_item = MenuItem::with_id(
        app,
        "about",
        format!("About {}", about::APP_NAME),
        true,
        None::<&str>,
    )?;

    let check_updates_item = MenuItem::with_id(
        app,
        "check-for-updates",
        "Check for Updates…",
        true,
        None::<&str>,
    )?;

    let help_menu = Submenu::with_id_and_items(app, "help", "Help", true, &[])?;

    Menu::with_items(
        app,
        &[
            &Submenu::with_items(
                app,
                about::APP_NAME,
                true,
                &[
                    &about_item,
                    &check_updates_item,
                    &PredefinedMenuItem::separator(app)?,
                    &PredefinedMenuItem::services(app, None)?,
                    &PredefinedMenuItem::separator(app)?,
                    &PredefinedMenuItem::hide(app, None)?,
                    &PredefinedMenuItem::hide_others(app, None)?,
                    &PredefinedMenuItem::separator(app)?,
                    &PredefinedMenuItem::quit(app, None)?,
                ],
            )?,
            &Submenu::with_items(
                app,
                "File",
                true,
                &[&PredefinedMenuItem::close_window(app, None)?],
            )?,
            &Submenu::with_items(
                app,
                "Edit",
                true,
                &[
                    &PredefinedMenuItem::undo(app, None)?,
                    &PredefinedMenuItem::redo(app, None)?,
                    &PredefinedMenuItem::separator(app)?,
                    &PredefinedMenuItem::cut(app, None)?,
                    &PredefinedMenuItem::copy(app, None)?,
                    &PredefinedMenuItem::paste(app, None)?,
                    &PredefinedMenuItem::select_all(app, None)?,
                ],
            )?,
            &Submenu::with_items(
                app,
                "View",
                true,
                &[&PredefinedMenuItem::fullscreen(app, None)?],
            )?,
            &window_menu,
            &help_menu,
        ],
    )
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let builder = tauri::Builder::default().plugin(tauri_plugin_opener::init());
    // macOS restores the window frame through AppKit autosave (window_frame.rs).
    #[cfg(not(target_os = "macos"))]
    let builder = builder.plugin(
        tauri_plugin_window_state::Builder::default()
            // The window starts hidden and window_frame::attach shows it.
            .with_state_flags(
                tauri_plugin_window_state::StateFlags::all()
                    & !tauri_plugin_window_state::StateFlags::VISIBLE,
            )
            .build(),
    );

    #[cfg(target_os = "macos")]
    let builder = builder
        .menu(build_menu)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "about" => about::show(&app.package_info().version.to_string()),
            "check-for-updates" => {
                let _ = app.emit("check-for-updates", ());
            }
            _ => {}
        });

    builder
        .setup(|app| {
            window_frame::attach(app.handle());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::list_state,
            commands::create_environment,
            commands::copy_environment_from_current,
            commands::environment_disk_usage,
            commands::rename_environment,
            commands::delete_environment,
            commands::create_account,
            commands::delete_account,
            commands::rename_account,
            commands::launch,
            commands::quit_cursor_cmd,
            commands::force_quit_cursor_cmd,
            commands::is_cursor_running,
            updates::check_for_updates,
            updates::open_url,
            usage::account_usage,
            about::about_info,
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app, event| {
            window_frame::handle_run_event(app, &event);
        });
}
