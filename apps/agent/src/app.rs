//! Tauri shell: tray icon, global hotkeys, settings window and the commands the page calls.
//! All logic lives in `ssc-pipeline` (and `settings.rs` for UI rules); this file is glue.

use crate::slot::Slot;
use crate::{permissions, settings};
use serde::Serialize;
use ssc_core::config::{Config, data_dir};
use ssc_core::protocol::SessionEndReason;
use ssc_core::secrets::{KeychainStore, Secret, SecretStore};
use ssc_core::types::{AboutMe, Profile};
use ssc_pipeline::system::{self, System};
use ssc_pipeline::transport::{revoke_device, store_device};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tauri::menu::{Menu, MenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Manager, State};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutState};

const TRAY_ID: &str = "ssc";

pub struct AppState {
    cfg: Mutex<Config>,
    /// H3: readers clone the `Arc` out; nothing holds a lock across an await on the system.
    system: Slot<System>,
    secrets: Arc<dyn SecretStore>,
    tray_visible: AtomicBool,
    last_error: Mutex<Option<String>>,
    /// Model download progress for the settings page ("whisper: 40%"), None when idle.
    download: Arc<Mutex<Option<String>>>,
    downloading: AtomicBool,
    handle: std::sync::OnceLock<AppHandle>,
}

type Res<T> = Result<T, String>;

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

impl AppState {
    async fn start_system(&self) {
        let _l = self.system.lifecycle().await;
        self.start_locked().await;
    }

    async fn stop_system(&self) {
        let _l = self.system.lifecycle().await;
        self.stop_locked().await;
    }

    async fn restart(&self) {
        let _l = self.system.lifecycle().await;
        self.stop_locked().await;
        self.start_locked().await;
    }

    /// Caller holds the lifecycle lock.
    async fn start_locked(&self) {
        let cfg = self.cfg.lock().unwrap().clone();
        let dl = self.download.clone();
        let progress = move |p: ssc_memory::download::Progress<'_>| {
            *dl.lock().unwrap() = Some(progress_text("embedder", &p));
        };
        let r = system::build(cfg.clone(), self.secrets.clone(), &progress).await;
        *self.download.lock().unwrap() = None;
        match r {
            Ok(s) => {
                *self.last_error.lock().unwrap() = None;
                let need_whisper = system::needs_whisper_download(&s);
                let s = Arc::new(s);
                // CR-23: agent.status for the phone. Weak: the pipeline lives inside the system.
                let weak = Arc::downgrade(&s);
                let lan: crate::health::LanStatusFn =
                    Arc::new(move || weak.upgrade().map_or(ssc_transport::LanTlsStatus::Off, |s| s.lan_tls.status()));
                s.pipeline.set_status_provider(crate::health::provider(cfg.clone(), self.secrets.clone(), lan));
                ssc_core::logbus::info("ssc-agent", format!("agent started ({:?} mode)", cfg.mode));
                if let Some(old) = self.system.replace(Some(s)) {
                    shutdown(&self.secrets, old).await;
                }
                if need_whisper {
                    self.spawn_whisper_download();
                }
            }
            Err(e) => {
                ssc_core::logbus::error("ssc-agent", format!("agent failed to start: {e}"));
                *self.last_error.lock().unwrap() = Some(e.to_string());
                if let Some(old) = self.system.take() {
                    shutdown(&self.secrets, old).await;
                }
            }
        }
    }

    /// Caller holds the lifecycle lock. The slot is emptied first, so readers see "not running"
    /// at once instead of waiting for the shutdown (H3).
    async fn stop_locked(&self) {
        let taken = self.system.take();
        if let Some(s) = taken {
            shutdown(&self.secrets, s).await;
        }
    }

    /// Fetch the whisper model in the background, then restart so STT picks it up.
    fn spawn_whisper_download(&self) {
        let Some(app) = self.handle.get().cloned() else { return };
        if self.downloading.swap(true, Ordering::SeqCst) {
            return;
        }
        tauri::async_runtime::spawn(async move {
            let st = app.state::<AppState>();
            let dl = st.download.clone();
            let progress = move |p: ssc_memory::download::Progress<'_>| {
                *dl.lock().unwrap() = Some(progress_text("whisper", &p));
            };
            let r = system::download_whisper(&progress).await;
            st.downloading.store(false, Ordering::SeqCst);
            match r {
                Ok(_) => {
                    *st.download.lock().unwrap() = None;
                    // Don't cut a running call short; the next restart picks the model up.
                    let capturing = st.system.get().is_some_and(|s| s.pipeline.is_capturing());
                    if !capturing {
                        st.restart().await;
                    }
                }
                Err(e) => *st.download.lock().unwrap() = Some(format!("whisper download failed: {e}")),
            }
        });
    }
}

async fn shutdown(secrets: &Arc<dyn SecretStore>, s: Arc<System>) {
    s.pipeline.shutdown().await;
    if let Some(dev) = s.transport.device_snapshot().await {
        let _ = store_device(&**secrets, &data_dir(), &dev);
    }
    s.transport.shutdown();
    s.background.cancel();
}

fn progress_text(what: &str, p: &ssc_memory::download::Progress<'_>) -> String {
    if p.total > 0 {
        format!("Downloading {what} model ({}): {}%", p.file, p.done * 100 / p.total)
    } else {
        format!("Downloading {what} model ({})…", p.file)
    }
}

// ---------- commands ----------

#[derive(Serialize)]
struct KeyView {
    name: &'static str,
    label: &'static str,
    set: bool,
}

#[derive(Serialize)]
struct StateView {
    config: Config,
    running: bool,
    error: Option<String>,
    capturing: bool,
    session_id: Option<String>,
    stt: String,
    screen_available: bool,
    keys: Vec<KeyView>,
    phone: Option<String>,
    link: Option<String>,
    debug_build: bool,
    tray_visible: bool,
    version: String,
    /// LAN TLS (CR-4) status from `LanTlsManager`.
    lan_tls: Option<ssc_transport::LanTlsStatus>,
    download: Option<String>,
}

#[tauri::command]
async fn get_state(st: State<'_, AppState>) -> Res<StateView> {
    let config = st.cfg.lock().unwrap().clone();
    let sys = st.system.get();
    let keys = settings::UI_SECRETS
        .iter()
        .map(|(name, label)| KeyView {
            name,
            label,
            set: settings::secret_key(name)
                .and_then(|k| st.secrets.get(&k).ok().flatten())
                .is_some_and(|s| !s.expose().is_empty()),
        })
        .collect();
    let (phone, link) = match sys.as_ref() {
        Some(s) => {
            let status = s.transport.status();
            let phone = s.transport.device_snapshot().await.map(|d| d.device_name);
            let link = Some(format!(
                "{} · {}{}",
                match status.path {
                    Some(p) => format!("{p:?}"),
                    None => "no link".into(),
                },
                if status.phone_live { "phone online" } else { "phone offline" },
                status.rtt_ms.map(|r| format!(" · RTT {r} ms")).unwrap_or_default()
            ));
            (phone, link)
        }
        None => (None, None),
    };
    Ok(StateView {
        running: sys.is_some(),
        error: st.last_error.lock().unwrap().clone(),
        capturing: sys.as_ref().is_some_and(|s| s.pipeline.is_capturing()),
        session_id: sys.as_ref().and_then(|s| s.pipeline.session_id()).map(|u| u.to_string()),
        stt: sys.as_ref().map(|s| format!("{:?}", s.stt)).unwrap_or_default(),
        screen_available: sys.as_ref().is_some_and(|s| s.screen_available),
        config,
        keys,
        phone,
        link,
        debug_build: settings::TESTING_ALLOWED,
        tray_visible: st.tray_visible.load(Ordering::SeqCst),
        version: crate::cli::version(),
        lan_tls: sys.as_ref().map(|s| s.lan_tls.status()),
        download: st.download.lock().unwrap().clone(),
    })
}

#[tauri::command]
async fn save_config(st: State<'_, AppState>, config: Config) -> Res<()> {
    let c = settings::sanitize(config)?;
    c.save(&Config::default_path()).map_err(err)?;
    *st.cfg.lock().unwrap() = c;
    st.restart().await;
    Ok(())
}

#[tauri::command]
async fn set_secret(st: State<'_, AppState>, name: String, value: String) -> Res<()> {
    let key = settings::secret_key(&name).ok_or("unknown key")?;
    let v = value.trim();
    if v.is_empty() {
        st.secrets.delete(&key).map_err(err)?;
    } else {
        st.secrets.set(&key, &Secret::new(v)).map_err(err)?;
    }
    st.restart().await;
    Ok(())
}

async fn memory(st: &AppState) -> Res<Arc<ssc_memory::SqliteMemory>> {
    st.system.get().map(|s| s.memory.clone()).ok_or_else(|| "agent not running".into())
}

#[tauri::command]
async fn get_about_me(st: State<'_, AppState>) -> Res<Option<AboutMe>> {
    memory(&st).await?.about_me().map_err(err)
}

#[tauri::command]
async fn set_about_me(st: State<'_, AppState>, about: AboutMe) -> Res<()> {
    memory(&st).await?.set_about_me(&about).map(|_| ()).map_err(err)
}

#[tauri::command]
async fn list_profiles(st: State<'_, AppState>) -> Res<Vec<Profile>> {
    memory(&st).await?.profiles().map_err(err)
}

#[tauri::command]
async fn put_profile(st: State<'_, AppState>, profile: Profile) -> Res<()> {
    if profile.id.trim().is_empty() || profile.name.trim().is_empty() {
        return Err("Profile needs an id and a name.".into());
    }
    memory(&st).await?.put_profile(&profile).map(|_| ()).map_err(err)
}

#[derive(Serialize)]
struct DocView {
    id: String,
    name: String,
    chunks: usize,
    added_at: u64,
}

#[tauri::command]
async fn list_docs(st: State<'_, AppState>, profile: String) -> Res<Vec<DocView>> {
    let docs = memory(&st).await?.docs(&profile).map_err(err)?;
    Ok(docs
        .into_iter()
        .map(|d| DocView { id: d.id.to_string(), name: d.name, chunks: d.chunks, added_at: d.added_at })
        .collect())
}

#[tauri::command]
async fn upload_doc(st: State<'_, AppState>, profile: String, name: String, bytes: Vec<u8>) -> Res<usize> {
    let m = memory(&st).await?;
    // Parsing + embedding is CPU work: keep it off the UI thread.
    tauri::async_runtime::spawn_blocking(move || m.ingest_doc(&profile, &name, &bytes))
        .await
        .map_err(err)?
        .map(|d| d.chunks)
        .map_err(err)
}

#[tauri::command]
async fn delete_doc(st: State<'_, AppState>, id: String) -> Res<()> {
    let id = uuid::Uuid::parse_str(&id).map_err(err)?;
    memory(&st).await?.delete_doc(id).map_err(err)
}

async fn toggle_capture_inner(app: &AppHandle, profile: Option<String>) -> Res<bool> {
    let st = app.state::<AppState>();
    let s = st.system.get().ok_or("agent not running")?;
    let capturing = if s.pipeline.is_capturing() {
        let p = s.pipeline.clone();
        // Summary + follow-up can take a while: don't hold the UI.
        tauri::async_runtime::spawn(async move {
            if let Err(e) = p.end_session(SessionEndReason::User).await {
                ssc_core::logbus::error("ssc-agent", format!("end session failed: {e}"));
            }
        });
        false
    } else {
        s.pipeline.start_session(profile).await.map_err(err)?;
        true
    };
    update_tray(app, capturing);
    Ok(capturing)
}

#[tauri::command]
async fn toggle_capture(app: AppHandle, profile: Option<String>) -> Res<bool> {
    toggle_capture_inner(&app, profile.filter(|p| !p.is_empty())).await
}

#[tauri::command]
async fn answer_now(st: State<'_, AppState>) -> Res<Option<String>> {
    Ok(st.system.get().and_then(|s| s.pipeline.answer_now()).map(|u| u.to_string()))
}

#[tauri::command]
async fn begin_pairing(st: State<'_, AppState>) -> Res<String> {
    // H3: a clone, so the up-to-90 s wait below never blocks hotkeys or get_state.
    let s = st.system.get().ok_or("agent not running")?;
    // The QR carries the LAN URL: wait for LAN TLS to settle so it is wss:// when available.
    ssc_pipeline::transport::wait_lan_tls_settled(&s.lan_tls, std::time::Duration::from_secs(90)).await;
    let qr = s.transport.begin_pairing().await.map_err(err)?;
    let pwa_url = st.cfg.lock().expect("lock").pwa_url.clone();
    ssc_crypto::qr::svg(&qr, pwa_url.as_deref()).map_err(err)
}

#[tauri::command]
async fn unpair(st: State<'_, AppState>) -> Res<()> {
    let _l = st.system.lifecycle().await;
    st.stop_locked().await;
    let revoked = revoke_device(&*st.secrets, &data_dir()).map_err(err);
    st.start_locked().await;
    revoked
}

#[tauri::command]
async fn debug_stats(st: State<'_, AppState>) -> Res<Vec<ssc_pipeline::StageStat>> {
    Ok(st.system.get().map(|s| s.pipeline.stage_stats()).unwrap_or_default())
}

#[tauri::command]
fn permission_checklist() -> Vec<permissions::Item> {
    permissions::checklist()
}

#[tauri::command]
fn request_permission(id: String) -> Res<()> {
    permissions::request(&id)
}

// ---------- tray + hotkeys ----------

fn update_tray(app: &AppHandle, capturing: bool) {
    if let Some(t) = app.tray_by_id(TRAY_ID) {
        let _ = t.set_tooltip(Some(if capturing { "Copilot: capturing" } else { "Copilot: idle" }));
    }
    if let Some(item) = app.try_state::<CaptureItem>() {
        let _ = item.0.set_text(if capturing { "Stop capture" } else { "Start capture" });
    }
}

struct CaptureItem(MenuItem<tauri::Wry>);

fn toggle_tray(app: &AppHandle) {
    let st = app.state::<AppState>();
    let visible = !st.tray_visible.load(Ordering::SeqCst);
    if let Some(t) = app.tray_by_id(TRAY_ID) {
        let _ = t.set_visible(visible);
        st.tray_visible.store(visible, Ordering::SeqCst);
    }
}

fn show_settings(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.set_focus();
    }
}

fn on_hotkey(app: &AppHandle, action: Hotkey) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        match action {
            Hotkey::Capture => {
                let _ = toggle_capture_inner(&app, None).await;
            }
            Hotkey::Answer => {
                let st = app.state::<AppState>();
                if let Some(s) = st.system.get() {
                    s.pipeline.answer_now();
                }
            }
            Hotkey::Screen => {
                let st = app.state::<AppState>();
                if let Some(s) = st.system.get() {
                    s.pipeline.answer_from_screen();
                }
            }
            Hotkey::Tray => toggle_tray(&app),
        }
    });
}

#[derive(Clone, Copy)]
enum Hotkey {
    Capture,
    Answer,
    /// CR-20: grab the focused window and answer what is on it.
    Screen,
    Tray,
}

fn register_hotkeys(app: &AppHandle, cfg: &Config) -> Vec<(Shortcut, Hotkey)> {
    let mut out = Vec::new();
    for (spec, action) in [
        (&cfg.hotkeys.capture_toggle, Hotkey::Capture),
        (&cfg.hotkeys.answer_now, Hotkey::Answer),
        (&cfg.hotkeys.screen_answer, Hotkey::Screen),
        (&cfg.hotkeys.tray_toggle, Hotkey::Tray),
    ] {
        match spec.parse::<Shortcut>() {
            Ok(s) => match app.global_shortcut().register(s) {
                Ok(()) => out.push((s, action)),
                Err(e) => ssc_core::logbus::error("ssc-agent", format!("hotkey {spec} could not be registered: {e}")),
            },
            Err(e) => ssc_core::logbus::error("ssc-agent", format!("hotkey {spec} invalid: {e}")),
        }
    }
    out
}

pub fn run() {
    let cfg = Config::load(&Config::default_path()).unwrap_or_else(|e| {
        ssc_core::logbus::warn("ssc-agent", format!("config invalid, using defaults: {e}"));
        Config::default()
    });
    let state = AppState {
        cfg: Mutex::new(cfg.clone()),
        system: Slot::default(),
        secrets: Arc::new(KeychainStore),
        tray_visible: AtomicBool::new(true),
        last_error: Mutex::new(None),
        download: Arc::default(),
        downloading: AtomicBool::new(false),
        handle: std::sync::OnceLock::new(),
    };
    let hotkeys: Arc<Mutex<Vec<(Shortcut, Hotkey)>>> = Arc::default();
    let hk = hotkeys.clone();
    tauri::Builder::default()
        // Must be the first plugin. A second launch (e.g. from a terminal while the menu-bar
        // instance runs) would fail to bind the LAN port; instead it hands over and exits, and
        // the running instance shows its window.
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| show_settings(app)))
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(move |app, shortcut, event| {
                    if event.state() != ShortcutState::Pressed {
                        return;
                    }
                    let action = hk.lock().unwrap().iter().find(|(s, _)| s == shortcut).map(|(_, a)| *a);
                    if let Some(a) = action {
                        on_hotkey(app, a);
                    }
                })
                .build(),
        )
        .manage(state)
        .invoke_handler(tauri::generate_handler![
            get_state,
            save_config,
            set_secret,
            get_about_me,
            set_about_me,
            list_profiles,
            put_profile,
            list_docs,
            upload_doc,
            delete_doc,
            toggle_capture,
            answer_now,
            begin_pairing,
            unpair,
            debug_stats,
            permission_checklist,
            request_permission
        ])
        .setup(move |app| {
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            let capture = MenuItem::with_id(app, "capture", "Start capture", true, None::<&str>)?;
            let answer = MenuItem::with_id(app, "answer", "Answer now", true, None::<&str>)?;
            let screen = MenuItem::with_id(app, "screen", "Answer from screen", true, None::<&str>)?;
            let open = MenuItem::with_id(app, "settings", "Settings…", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&capture, &answer, &screen, &open, &quit])?;
            app.manage(CaptureItem(capture));
            let mut tray = TrayIconBuilder::with_id(TRAY_ID).tooltip("Copilot: idle").menu(&menu).on_menu_event(
                |app, ev| match ev.id.as_ref() {
                    "capture" => on_hotkey(app, Hotkey::Capture),
                    "answer" => on_hotkey(app, Hotkey::Answer),
                    "screen" => on_hotkey(app, Hotkey::Screen),
                    "settings" => show_settings(app),
                    "quit" => {
                        let app = app.clone();
                        tauri::async_runtime::spawn(async move {
                            app.state::<AppState>().stop_system().await;
                            app.exit(0);
                        });
                    }
                    _ => {}
                },
            );
            if let Some(icon) = app.default_window_icon() {
                tray = tray.icon(icon.clone());
            }
            tray.build(app)?;

            *hotkeys.lock().unwrap() = register_hotkeys(app.handle(), &cfg);

            // Closing the window hides it; the agent keeps running in the tray.
            if let Some(w) = app.get_webview_window("main") {
                let w2 = w.clone();
                w.on_window_event(move |e| {
                    if let tauri::WindowEvent::CloseRequested { api, .. } = e {
                        api.prevent_close();
                        let _ = w2.hide();
                    }
                });
            }
            // A launch is always by hand (no login item), and as a menu-bar-only app nothing would
            // be visible otherwise: show the settings window. Closing it hides to the tray.
            show_settings(app.handle());

            let handle = app.handle().clone();
            let _ = handle.state::<AppState>().handle.set(handle.clone());
            tauri::async_runtime::spawn(async move {
                handle.state::<AppState>().start_system().await;
            });
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("tauri app failed");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_hotkeys_parse() {
        let h = Config::default().hotkeys;
        for spec in [&h.capture_toggle, &h.answer_now, &h.screen_answer, &h.tray_toggle] {
            assert!(spec.parse::<Shortcut>().is_ok(), "{spec}");
        }
    }
}
