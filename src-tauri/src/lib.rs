mod app;
mod assoc;
mod catch;
mod clipboard;
mod curl;
pub mod engine;
mod filename;
mod grab;
mod media;
mod motw;
mod proc;
pub mod store;
mod tools;
mod torrent;

use app::{App, Submitted};
use std::sync::Arc;
use std::time::Duration;
use store::{Settings, View};
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, RunEvent, State, WindowEvent};
use tauri_plugin_opener::OpenerExt;

type S<'a> = State<'a, Arc<App>>;

#[derive(serde::Serialize)]
struct Boot {
    items: Vec<View>,
    settings: Settings,
    version: &'static str,
}

#[tauri::command]
fn boot(app: S) -> Boot {
    Boot { items: app.views(), settings: app.settings(), version: env!("CARGO_PKG_VERSION") }
}

#[tauri::command]
async fn submit(app: S<'_>, text: String) -> Result<Submitted, String> {
    app.inner().clone().submit(text).await
}

#[tauri::command]
fn add_links(app: S, urls: Vec<String>, headers: Vec<(String, String)>) -> Vec<u64> {
    app.inner().add_links(urls, headers)
}

#[tauri::command]
fn add_video(app: S, token: String, height: Option<u32>, audio: bool) -> Result<u64, String> {
    app.inner().add_video(token, height, audio)
}

#[tauri::command]
fn add_stream(app: S, token: String, choice: Option<u64>) -> Result<u64, String> {
    app.inner().add_stream(token, choice)
}

#[tauri::command]
async fn add_torrent(app: S<'_>, token: String, files: Option<Vec<usize>>) -> Result<u64, String> {
    app.inner().add_torrent(token, files).await
}

#[tauri::command]
async fn pause(app: S<'_>, id: u64) -> Result<(), ()> {
    app.inner().pause(id).await;
    Ok(())
}

#[tauri::command]
async fn resume(app: S<'_>, id: u64) -> Result<(), ()> {
    app.inner().resume(id).await;
    Ok(())
}

#[tauri::command]
async fn remove(app: S<'_>, id: u64, delete: bool) -> Result<(), ()> {
    app.inner().remove(id, delete).await;
    Ok(())
}

#[tauri::command]
fn relink(app: S, id: u64, text: String) -> Result<(), String> {
    app.inner().relink(id, text)
}

#[tauri::command]
fn open(handle: AppHandle, app: S, id: u64) -> Result<(), String> {
    let p = app.path_of(id).ok_or("Not found")?;
    handle.opener().open_path(p.to_string_lossy(), None::<&str>).map_err(|e| e.to_string())
}

#[tauri::command]
fn reveal(handle: AppHandle, app: S, id: u64) -> Result<(), String> {
    let p = app.path_of(id).ok_or("Not found")?;
    if p.is_dir() {
        handle.opener().open_path(p.to_string_lossy(), None::<&str>).map_err(|e| e.to_string())
    } else {
        handle.opener().reveal_item_in_dir(p).map_err(|e| e.to_string())
    }
}

#[tauri::command]
fn open_folder(handle: AppHandle, app: S) -> Result<(), String> {
    handle.opener().open_path(app.settings().dir.to_string_lossy(), None::<&str>).map_err(|e| e.to_string())
}

#[tauri::command]
fn set_settings(app: S, settings: Settings) -> Settings {
    app.inner().set_settings(settings)
}

#[tauri::command]
async fn probe_sizes(app: S<'_>, urls: Vec<String>, headers: Vec<(String, String)>) -> Result<Vec<Option<u64>>, ()> {
    Ok(app.probe_sizes(urls, headers).await)
}

#[tauri::command]
fn clip_peek(app: S) -> Option<app::Clip> {
    app.clip_peek()
}

#[tauri::command]
async fn clip_take(handle: AppHandle, app: S<'_>) -> Result<Submitted, String> {
    let text = app.clip_text().ok_or("Nothing copied")?;
    let r = app.inner().clone().submit(text).await;
    match &r {
        Ok(Submitted::Added { .. }) => {}
        Ok(s) => {
            show_main(&handle);
            let _ = handle.emit_to("main", "sheet", s.clone());
        }
        Err(e) => {
            show_main(&handle);
            let _ = handle.emit_to("main", "problem", e.clone());
        }
    }
    r
}

#[tauri::command]
fn clip_ignore(app: S, text: String) {
    app.clip_ignore(text);
}

#[tauri::command]
async fn submit_bytes(app: S<'_>, name: String, data: Vec<u8>) -> Result<Submitted, String> {
    app.inner().clone().submit_bytes(name, data).await
}

#[tauri::command]
fn catch_hide(handle: AppHandle) {
    catch::hide(&handle);
}

#[tauri::command]
fn show_window(handle: AppHandle) {
    show_main(&handle);
}

#[tauri::command]
async fn quit(handle: AppHandle, app: S<'_>) -> Result<(), ()> {
    app.quit().await;
    handle.exit(0);
    Ok(())
}

fn show_main(handle: &AppHandle) {
    if let Some(w) = handle.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}

fn args_in(handle: &AppHandle, args: Vec<String>) {
    let app = handle.state::<Arc<App>>().inner().clone();
    for a in args.into_iter().filter(|a| !a.starts_with("--")) {
        let app = app.clone();
        let h = handle.clone();
        tauri::async_runtime::spawn(async move {
            match app.submit(a).await {
                Ok(Submitted::Nothing { .. }) | Ok(Submitted::Added { .. }) => {}
                Ok(s) => {
                    show_main(&h);
                    let _ = h.emit_to("main", "sheet", s);
                }
                Err(e) => {
                    let _ = h.emit_to("main", "problem", e);
                }
            }
        });
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|handle, argv, _cwd| {
            show_main(handle);
            args_in(handle, argv.into_iter().skip(1).collect());
        }))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_notification::init())
        .invoke_handler(tauri::generate_handler![
            boot,
            submit,
            add_links,
            add_video,
            add_stream,
            add_torrent,
            pause,
            resume,
            remove,
            relink,
            open,
            reveal,
            open_folder,
            set_settings,
            probe_sizes,
            clip_peek,
            clip_take,
            clip_ignore,
            submit_bytes,
            catch_hide,
            show_window,
            quit
        ])
        .setup(|a| {
            let handle = a.handle().clone();
            let data = a.path().app_local_data_dir()?;
            let app = App::new(handle.clone(), data);
            a.manage(app.clone());
            catch::create(&handle);

            let show = MenuItem::with_id(a, "show", "Open", true, None::<&str>)?;
            let quit = MenuItem::with_id(a, "quit", "Quit", true, None::<&str>)?;
            let menu = Menu::with_items(a, &[&show, &quit])?;
            TrayIconBuilder::with_id("tray")
                .icon(tauri::image::Image::from_bytes(include_bytes!("../icons/tray.png"))?)
                .tooltip("Dynamic Downloading Dashboard")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|h, e| match e.id.as_ref() {
                    "show" => show_main(h),
                    "quit" => {
                        let h = h.clone();
                        tauri::async_runtime::spawn(async move {
                            let app = h.state::<Arc<App>>().inner().clone();
                            app.quit().await;
                            h.exit(0);
                        });
                    }
                    _ => {}
                })
                .on_tray_icon_event(|t, e| {
                    if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = e {
                        show_main(t.app_handle());
                    }
                })
                .build(a)?;

            let ticker = app.clone();
            tauri::async_runtime::spawn(async move {
                let mut n = 0u64;
                let mut tick = tokio::time::interval(Duration::from_millis(250));
                loop {
                    tick.tick().await;
                    ticker.tick();
                    n += 1;
                    if n % 4 == 0 {
                        let t = ticker.clone();
                        let _ = tauri::async_runtime::spawn_blocking(move || t.persist()).await;
                    }
                }
            });

            let watcher = app.clone();
            clipboard::watch(move |text| watcher.on_clip(text));

            let booting = app.clone();
            tauri::async_runtime::spawn(async move {
                booting.pump();
                booting.boot_torrents().await;
            });

            if app.settings().magnets {
                assoc::set(true);
            }

            let h = handle.clone();
            tauri::async_runtime::spawn(async move {
                tokio::time::sleep(Duration::from_millis(1500)).await;
                if let Some(w) = h.get_webview_window("main") {
                    if !w.is_visible().unwrap_or(true) {
                        let _ = w.show();
                    }
                }
            });
            args_in(&handle, std::env::args().skip(1).collect());
            Ok(())
        })
        .on_window_event(|w, e| {
            if w.label() != "main" {
                return;
            }
            if let WindowEvent::CloseRequested { api, .. } = e {
                api.prevent_close();
                let h = w.app_handle().clone();
                let app = h.state::<Arc<App>>().inner().clone();
                if app.has_active() {
                    let _ = w.hide();
                } else {
                    tauri::async_runtime::spawn(async move {
                        app.quit().await;
                        h.exit(0);
                    });
                }
            }
        })
        .build(tauri::generate_context!())
        .expect("error while starting Dynamic Downloading Dashboard")
        .run(|h, e| {
            if let RunEvent::ExitRequested { code: None, api, .. } = e {
                api.prevent_exit();
                let _ = h;
            }
        });
}
