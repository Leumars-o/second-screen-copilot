//! W2-HARDEN: the settings window's IPC surface (`docs/security/threat-model.md` §9).
//!
//! `ssc-agent` is a binary crate, so these are source-level guards over `src/app.rs`,
//! `src/settings.rs`, the Tauri config and the page script. They fail when the surface grows
//! (a new command, a new plugin permission, a weaker CSP, a command that returns secret
//! material), so every change to it gets a security review.

use std::path::Path;

fn read(rel: &str) -> String {
    std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(rel)).unwrap()
}

/// The commands a page script can invoke. None of them returns a key: keys are write-only from
/// the page (`set_secret`) and `get_state` reports only whether each one is set.
const COMMANDS: &[&str] = &[
    "get_state",
    "save_config",
    "set_secret",
    "get_about_me",
    "set_about_me",
    "list_profiles",
    "put_profile",
    "list_docs",
    "upload_doc",
    "delete_doc",
    "toggle_capture",
    "answer_now",
    "begin_pairing",
    "unpair",
    "debug_stats",
    "permission_checklist",
    "request_permission",
];

fn handler_list(app: &str) -> Vec<String> {
    let start = app.find("generate_handler![").expect("invoke handler") + "generate_handler![".len();
    let end = start + app[start..].find(']').unwrap();
    app[start..end].split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
}

/// Every `#[tauri::command]` fn: (name, signature up to the opening brace).
fn commands(app: &str) -> Vec<(String, String)> {
    app.split("#[tauri::command]")
        .skip(1)
        .map(|chunk| {
            let sig = &chunk[..chunk.find('{').unwrap()];
            let name = sig.split("fn ").nth(1).unwrap().split('(').next().unwrap().trim().to_string();
            (name, sig.to_string())
        })
        .collect()
}

fn struct_body<'a>(src: &'a str, name: &str) -> &'a str {
    let i = src.find(&format!("struct {name} {{")).unwrap_or_else(|| panic!("struct {name}"));
    let body = &src[i..];
    &body[..body.find("\n}").unwrap()]
}

#[test]
fn ipc_command_list_is_the_reviewed_one() {
    let app = read("src/app.rs");
    let registered = handler_list(&app);
    assert_eq!(registered, COMMANDS, "IPC surface changed: review the new command, then update COMMANDS");
    let defined: Vec<String> = commands(&app).into_iter().map(|(n, _)| n).collect();
    let mut a = defined.clone();
    let mut b = registered.clone();
    a.sort();
    b.sort();
    assert_eq!(a, b, "every #[tauri::command] is registered and vice versa");
}

#[test]
fn no_command_returns_secret_material() {
    let app = read("src/app.rs");
    for (name, sig) in commands(&app) {
        let ret = sig.split("->").nth(1).unwrap_or("");
        assert!(!ret.contains("Secret"), "{name} returns a Secret: {sig}");
    }
    // What `get_state` returns: keys only as a `set: bool`, config has no key fields.
    let key_view = struct_body(&app, "KeyView");
    assert!(key_view.contains("set: bool"), "{key_view}");
    for f in ["value", "secret", "token", "key:"] {
        assert!(!key_view.contains(f), "KeyView exposes {f}: {key_view}");
    }
    let state_view = struct_body(&app, "StateView");
    assert!(!state_view.contains("Secret"), "{state_view}");
    let config =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../crates/ssc-core/src/config.rs"))
            .unwrap();
    for line in config.lines().map(str::trim).filter(|l| l.starts_with("pub ") && l.contains(':')) {
        let field = line.trim_start_matches("pub ").split(':').next().unwrap().to_lowercase();
        for word in field.split('_') {
            assert!(
                !["key", "apikey", "secret", "password", "credential", "bearer"].contains(&word),
                "Config grew a secret-looking field: {line}"
            );
        }
    }
    // The only `.expose()` in the shell is the "is it set" check.
    assert_eq!(app.matches(".expose()").count(), 1);
    assert!(app.contains(".is_some_and(|s| !s.expose().is_empty())"));
}

#[test]
fn page_can_write_only_api_keys() {
    let settings = read("src/settings.rs");
    let body = &settings[settings.find("pub fn secret_key").unwrap()..];
    let body = &body[..body.find("\n}").unwrap()];
    for device_key in ["PcIdentity", "PairedDevice", "AcmeAccount", "LanTlsKey"] {
        assert!(!body.contains(device_key), "page could overwrite device key {device_key}");
    }
    let app = read("src/app.rs");
    let set_secret = &app[app.find("async fn set_secret").unwrap()..];
    let set_secret = &set_secret[..set_secret.find("\n}").unwrap()];
    assert!(set_secret.contains("settings::secret_key(&name).ok_or("), "set_secret must go through the allowlist");
}

#[test]
fn webview_permissions_and_csp_are_minimal() {
    let caps: serde_json::Value = serde_json::from_str(&read("capabilities/default.json")).unwrap();
    assert_eq!(caps["permissions"], serde_json::json!(["core:default"]), "no fs/shell/http/opener plugin access");
    assert_eq!(caps["windows"], serde_json::json!(["main"]));

    let conf: serde_json::Value = serde_json::from_str(&read("tauri.conf.json")).unwrap();
    let csp = conf["app"]["security"]["csp"].as_str().expect("a CSP is set");
    let directive = |name: &str| {
        csp.split(';').map(str::trim).find(|d| d.starts_with(name)).map(str::to_string).unwrap_or_default()
    };
    let script = directive("script-src");
    assert_eq!(script, "script-src 'self'", "page scripts: bundled file only");
    assert!(!csp.contains("unsafe-eval"));
    assert_eq!(directive("default-src"), "default-src 'self'", "no remote connect/frame/object sources");
    assert!(!csp.contains("http:") && !csp.contains("https:") && !csp.contains('*'), "no remote origins: {csp}");
    assert!(conf["app"]["security"].get("dangerousDisableAssetCspModification").is_none());
    assert!(conf["plugins"].is_null() || conf["plugins"].as_object().is_some_and(|p| p.is_empty()));
}

#[test]
fn page_script_has_no_html_injection_sinks_for_data() {
    let js = read("ui/app.js");
    for bad in ["eval(", "new Function", "document.write", "insertAdjacentHTML", "outerHTML"] {
        assert!(!js.contains(bad), "ui/app.js uses {bad}");
    }
    // innerHTML only for clearing, and for the QR SVG that ssc-crypto renders from the payload.
    for line in js.lines().filter(|l| l.contains("innerHTML")) {
        let l = line.trim();
        assert!(l == "text.innerHTML = \"\";" || l.starts_with("box.innerHTML = svg;"), "innerHTML sink: {l}");
    }
    let html = read("ui/index.html");
    assert!(!html.contains("<script>"), "inline script in index.html");
    // Inline event handlers (` onclick=` etc.): the CSP blocks them, but none should exist.
    let lower = html.to_lowercase();
    let handler = lower.match_indices(" on").any(|(i, _)| {
        let rest = &lower[i + 3..];
        let name: String = rest.chars().take_while(|c| c.is_ascii_alphabetic()).collect();
        !name.is_empty() && rest[name.len()..].trim_start().starts_with('=')
    });
    assert!(!handler, "inline event handler in index.html");
}
