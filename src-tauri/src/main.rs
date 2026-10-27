#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::{path::PathBuf, sync::Arc};
use tauri::{Emitter, Manager};
use uapz::app::AppService;

struct SmokeMode(bool);

#[tauri::command]
fn smoke_result(
    app: tauri::AppHandle,
    mode: tauri::State<'_, SmokeMode>,
    error: Option<String>,
) -> Result<(), String> {
    if !mode.0 {
        return Err("native smoke mode is not enabled".into());
    }
    match error {
        Some(error) => {
            eprintln!("native smoke failed: {error}");
            app.exit(1);
        }
        None => {
            println!(
                "native smoke passed: webview, IPC, graph save/load, run inputs, file review, execution and history"
            );
            app.exit(0);
        }
    }
    Ok(())
}

#[tauri::command]
fn quit(app: tauri::AppHandle) {
    // A programmatic exit carries a code, so the exit handler lets it through.
    app.exit(0);
}

#[tauri::command]
async fn request(
    service: tauri::State<'_, Arc<AppService>>,
    method: String,
    args: Option<serde_json::Value>,
) -> Result<serde_json::Value, String> {
    let service = Arc::clone(service.inner());
    tauri::async_runtime::spawn_blocking(move || {
        service.request(&method, args.unwrap_or_else(|| serde_json::json!({})))
    })
    .await
    .map_err(|error| error.to_string())?
}

fn workspace(app: &tauri::App) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args_os().collect();
    let explicit = args.iter().position(|arg| arg == "--workspace");
    let path = if let Some(index) = explicit {
        PathBuf::from(
            args.get(index + 1)
                .ok_or("--workspace requires a directory")?,
        )
    } else if let Some(path) = std::env::var_os("UAPZ_WORKSPACE_DIR") {
        PathBuf::from(path)
    } else if cfg!(debug_assertions) {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..")
    } else {
        app.path().app_local_data_dir()?.join("workspace")
    };
    std::fs::create_dir_all(&path)?;
    Ok(std::fs::canonicalize(path)?)
}

fn main() {
    let smoke = cfg!(debug_assertions) && std::env::args().any(|arg| arg == "--smoke-test");
    if smoke && !std::env::args().any(|arg| arg == "--workspace") {
        eprintln!("--smoke-test requires an explicit temporary --workspace directory");
        std::process::exit(2);
    }
    let app = tauri::Builder::default()
        .manage(SmokeMode(smoke))
        .setup(|app| {
            let root = workspace(app)?;
            // Legacy library helpers resolve relative paths; fix the workspace once,
            // before any worker starts. A running process never changes directories.
            std::env::set_current_dir(&root)?;
            app.manage(Arc::new(AppService::new(root)));
            Ok(())
        })
        .on_page_load(move |window, payload| {
            if smoke
                && matches!(payload.event(), tauri::webview::PageLoadEvent::Finished)
                && let Err(error) = window.eval(include_str!("smoke.js"))
            {
                eprintln!("native smoke injection failed: {error}");
                window.app_handle().exit(1);
            }
        })
        .on_window_event(move |window, event| {
            // The renderer owns the unsaved-changes prompt: it answers the event by
            // calling `quit`, or leaves the window open.
            if let tauri::WindowEvent::CloseRequested { api, .. } = event
                && !smoke
            {
                api.prevent_close();
                let _ = window.emit("close-requested", ());
            }
        })
        .invoke_handler(tauri::generate_handler![request, smoke_result, quit])
        .build(tauri::generate_context!())
        .expect("failed to run uapz");
    if smoke {
        let handle = app.handle().clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(30));
            eprintln!("native smoke timed out");
            handle.exit(1);
        });
    }
    app.run(move |app, event| {
        if let tauri::RunEvent::ExitRequested { code, api, .. } = &event {
            if code.is_none() && !smoke {
                // Quit from the menu or dock: let the renderer confirm unsaved work.
                api.prevent_exit();
                let _ = app.emit("close-requested", ());
                return;
            }
            let service = app.state::<Arc<AppService>>();
            service.cancel_active();
            // Give cancellable process workers time to terminate their subprocesses.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while std::time::Instant::now() < deadline
                && service
                    .request("runtime_state", serde_json::json!({}))
                    .is_ok_and(|state| state["running"] == true)
            {
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
        }
    });
}
