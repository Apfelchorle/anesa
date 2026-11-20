mod commands;
mod events;
mod player;
mod protocol;
mod security;
mod settings;
mod torrent;
mod tray;

use player::PlayerState;
use settings::{migrate_from_electron, AppSettings, SettingsState};
use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};
use tauri_plugin_opener::OpenerExt;
use torrent::TorrentState;

/// Origins that the main webview may navigate to. Everything else is either opened
/// externally (if allowlisted) or blocked.
fn is_internal_url(url: &url::Url) -> bool {
    // Tauri dev server (customize if devUrl changes) and production asset origins.
    match (url.scheme(), url.host_str()) {
        ("http", Some("localhost")) => true,
        ("http", Some("127.0.0.1")) => true,
        ("https", Some("tauri.localhost")) => true,
        ("http", Some("tauri.localhost")) => true,
        ("tauri", _) => true,
        _ => false,
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let builder = tauri::Builder::default()
        .plugin(tauri_plugin_log::Builder::new().build())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_store::Builder::default().build())
        .plugin(tauri_plugin_deep_link::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_single_instance::init(|app, argv, _cwd| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.show();
                let _ = window.unminimize();
                let _ = window.set_focus();
            }
            for arg in argv {
                if arg.starts_with("hayase://") {
                    protocol::handle_deep_link(app, &arg);
                }
            }
        }))
        .setup(|app| {
            // Load settings from the platform app-config dir, importing legacy Electron settings once.
            let config_dir = app.path().app_config_dir()?;
            std::fs::create_dir_all(&config_dir).ok();
            let settings_path = config_dir.join("settings.json");
            let mut initial = AppSettings::default();

            // Best-effort legacy import (Electron used `userData/settings.json`; Tauri's config dir
            // is close but not identical, so also try the Electron-style path).
            if let Ok(data_dir) = app.path().app_data_dir() {
                let legacy = data_dir.join("settings.json");
                if legacy.exists() {
                    migrate_from_electron(&legacy, &mut initial);
                }
            }
            let state = SettingsState::load(settings_path);
            if initial.player_path.is_some() || initial.torrent_path.is_some() {
                state.update(|s| {
                    s.player_path = s.player_path.clone().or(initial.player_path);
                    s.torrent_path = s.torrent_path.clone().or(initial.torrent_path);
                });
            }
            let settings_snapshot = state.get();

            // Start the in-process torrent engine before the webview can invoke commands.
            // Electron ran the torrent client in a utility process and defaulted to
            // `<tmp>/webtorrent` when no download path was configured; keep that default.
            let download_dir = settings_snapshot
                .torrent_path
                .as_deref()
                .filter(|p| !p.trim().is_empty())
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| std::env::temp_dir().join("webtorrent"));
            let torrent_data_dir = app
                .path()
                .app_data_dir()
                .unwrap_or_else(|_| config_dir.join("data"))
                .join("torrent");
            let library_path = torrent_data_dir.join("library.json");
            let cache_dir = torrent_data_dir.join("cache");

            let torrent_state = match tauri::async_runtime::block_on(torrent::start_rqbit(
                settings_snapshot.torrent_settings.clone(),
                download_dir,
                library_path,
                cache_dir,
            )) {
                Ok(backend) => {
                    log::info!("Torrent engine started");
                    TorrentState::new(backend)
                }
                Err(e) => {
                    log::error!("Failed to start torrent engine, using degraded stub: {e}");
                    TorrentState::default()
                }
            };

            app.manage(state);
            app.manage(torrent_state);
            app.manage(PlayerState::default());

            tray::setup_tray(app.handle())?;

            // Create the main window in Rust so we can install a strict navigation guard.
            let app_handle = app.handle().clone();
            let window = WebviewWindowBuilder::new(app, "main", WebviewUrl::App("index.html".into()))
                .title("Hayase")
                .inner_size(1600.0, 900.0)
                .min_inner_size(480.0, 360.0)
                .decorations(false)
                .shadow(true)
                .initialization_script(include_str!("../init-bridge.js"))
                .on_navigation(move |url| {
                    if is_internal_url(url) {
                        return true;
                    }
                    // Never navigate the privileged webview to arbitrary external origins.
                    // Open allowlisted links externally instead.
                    if security::validate_external_url(url.as_str()).is_ok() {
                        let _ = app_handle.opener().open_url(url.as_str(), None::<String>);
                    } else {
                        log::warn!("Blocked webview navigation to {url}");
                    }
                    false
                })
                .build()?;

            // Hide-to-tray behavior on close request.
            let app_handle = app.handle().clone();
            window.on_window_event(move |event| {
                if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                    if let Some(state) = app_handle.try_state::<SettingsState>() {
                        if state.get().hide_to_tray {
                            api.prevent_close();
                            if let Some(w) = app_handle.get_webview_window("main") {
                                let _ = w.hide();
                            }
                        }
                    }
                }
            });

            // Deep links (cold start + runtime). API varies slightly by plugin version.
            #[cfg(desktop)]
            {
                use tauri_plugin_deep_link::DeepLinkExt;
                let handle = app.handle().clone();
                app.deep_link().on_open_url(move |event| {
                    for url in event.urls() {
                        protocol::handle_deep_link(&handle, url.as_str());
                    }
                });
            }

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::version,
            commands::open_url,
            commands::window_minimise,
            commands::window_maximise,
            commands::window_fullscreen,
            commands::window_close,
            commands::window_focus,
            commands::app_restart,
            commands::app_quit,
            commands::set_zoom,
            commands::set_hide_to_tray,
            commands::select_player,
            commands::select_download,
            commands::spawn_player,
            commands::open_ui_devtools,
            commands::open_torrent_devtools,
            commands::get_settings,
            commands::update_settings,
            commands::set_doh,
            commands::set_angle,
            commands::get_logs,
            commands::get_device_info,
            commands::default_transparency,
            commands::accent_color,
            commands::check_update,
            commands::update_ready,
            commands::update_and_restart,
            commands::add_torrent,
            commands::torrent_active,
            commands::torrent_info,
            commands::torrent_file_info,
            commands::torrent_peer_info,
            commands::torrent_trackers,
            commands::torrent_protocol_status,
            commands::torrent_library,
            commands::torrent_cached,
            commands::torrent_rescan,
            commands::torrent_delete,
            commands::torrent_remove_background,
            commands::torrent_check_available_space,
            commands::torrent_check_incoming_connections,
            commands::torrent_create_nzb,
            commands::torrent_create_http_webseed,
            commands::torrent_attachments,
            commands::torrent_tracks,
            commands::torrent_chapters,
            commands::torrent_subtitles,
            commands::torrent_update_peer_counts,
            commands::torrent_download_progress,
            commands::debug,
            commands::profile,
        ]);

    builder
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
