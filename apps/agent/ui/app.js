// @ts-check
// Settings window. Plain ES module, no build step: every action is a Tauri command in
// src/app.rs. Secrets are write-only from here (the page only learns whether a key is set).

/** @type {(cmd: string, args?: Record<string, unknown>) => Promise<any>} */
// @ts-ignore - injected by Tauri (app.withGlobalTauri)
const invoke = window.__TAURI__.core.invoke;

const $ = (/** @type {string} */ s) => /** @type {HTMLElement} */ (document.querySelector(s));
/** @param {string} sel */
const form = (sel) => /** @type {HTMLFormElement} */ (document.querySelector(sel));

/** @type {any} */
let state = null;
/** @type {any[]} */
let profiles = [];

function showError(/** @type {unknown} */ e) {
  const b = $("#error");
  b.textContent = e ? String(e) : "";
  b.hidden = !e;
}

/** Run a command, show its error in the banner. */
async function call(/** @type {string} */ cmd, /** @type {Record<string, unknown>} */ args = {}) {
  try {
    const r = await invoke(cmd, args);
    showError(null);
    return r;
  } catch (e) {
    showError(e);
    throw e;
  }
}

function setField(/** @type {HTMLFormElement} */ f, /** @type {string} */ name, /** @type {any} */ v) {
  const el = /** @type {HTMLInputElement} */ (f.elements.namedItem(name));
  if (!el) return;
  if (el.type === "checkbox") el.checked = !!v;
  else el.value = v ?? "";
}
function getField(/** @type {HTMLFormElement} */ f, /** @type {string} */ name) {
  const el = /** @type {HTMLInputElement} */ (f.elements.namedItem(name));
  return el.type === "checkbox" ? el.checked : el.value;
}

async function refreshState() {
  state = await invoke("get_state");
  const st = $("#status");
  st.textContent = !state.running ? "not running" : state.capturing ? "capturing" : `ready · ${state.config.mode}`;
  st.className = "pill" + (state.capturing ? " on" : "");
  $("#version").textContent = state.version;
  if (state.error) showError(`Agent failed to start: ${state.error}`);
  $("#capture").textContent = state.capturing ? "Stop capture" : "Start capture";
  $("#capture-info").textContent = state.running
    ? `Speech-to-text: ${state.stt || "none"} · screen reader: ${state.screen_available ? "on" : "unavailable"}`
    : "";
  const h = state.config.hotkeys;
  $("#hotkeys").textContent = `Hotkeys: capture ${h.capture_toggle} · answer now ${h.answer_now} · hide tray ${h.tray_toggle}`;
  $("#phone-info").textContent = state.phone ? `Paired: ${state.phone} — ${state.link ?? ""}` : "No phone paired.";
  $("#lan-tls").textContent = lanTlsText(state.lan_tls);
  const dl = $("#download");
  dl.textContent = state.download ?? "";
  dl.hidden = !state.download;
  /** @type {HTMLButtonElement} */ ($("#unpair")).disabled = !state.phone;
  $("#testing-row").hidden = !state.debug_build;
}

/** LanTlsStatus from ssc-transport: {state: off|starting|active|failed, ...}. */
function lanTlsText(/** @type {any} */ t) {
  if (!t || t.state === "off") return "LAN: plain ws:// (set a LAN domain + Cloudflare token for wss://)";
  if (t.state === "starting") return "LAN TLS: setting up DNS and certificate… (pairing waits for it)";
  if (t.state === "active") {
    const until = new Date(t.not_after * 1000).toLocaleDateString();
    return `LAN TLS: wss://${t.domain}${t.ip ? ` → ${t.ip}` : ""} · certificate until ${until}${t.warning ? ` · warning: ${t.warning}` : ""}`;
  }
  return `LAN TLS failed at ${t.stage}: ${t.message} (using ws:// LAN + relay)`;
}

function renderSettings() {
  const f = form("#settings");
  const c = state.config;
  for (const k of ["mode", "redaction", "redact_emails", "keep_transcripts", "relay_url", "lan_domain", "lan_port", "testing"]) {
    setField(f, k, c[k]);
  }
  for (const k of ["gemini_tier", "strong_model", "strong_base_url", "decision_cloud_base_url"]) setField(f, k, c.models[k]);
}

function renderKeys() {
  const box = $("#keys");
  box.replaceChildren();
  for (const k of state.keys) {
    const row = document.createElement("div");
    row.className = "row";
    const label = document.createElement("label");
    label.textContent = `${k.label} ${k.set ? "(set)" : "(not set)"}`;
    const input = document.createElement("input");
    input.type = "password";
    input.autocomplete = "off";
    input.placeholder = k.set ? "•••••• (enter a new value to replace)" : "paste key";
    label.append(input);
    const save = document.createElement("button");
    save.textContent = "Save";
    save.onclick = async () => {
      await call("set_secret", { name: k.name, value: input.value });
      input.value = "";
      await refreshAll();
    };
    row.append(label, save);
    box.append(row);
  }
}

async function renderChecklist() {
  const items = await invoke("permission_checklist");
  const ul = $("#checklist");
  ul.replaceChildren();
  for (const p of items) {
    const li = document.createElement("li");
    const s = document.createElement("span");
    s.className = `state ${p.status}`;
    s.textContent = p.status === "granted" ? "✓ granted" : p.status === "missing" ? "missing" : "check";
    const text = document.createElement("span");
    text.innerHTML = "";
    const b = document.createElement("b");
    b.textContent = p.name;
    text.append(b, document.createTextNode(` — ${p.why}`));
    text.style.flex = "1";
    li.append(s, text);
    if (p.status !== "granted") {
      const btn = document.createElement("button");
      btn.className = "secondary";
      btn.textContent = "Open settings";
      btn.onclick = () => call("request_permission", { id: p.id });
      li.append(btn);
    }
    ul.append(li);
  }
}

async function renderProfiles() {
  if (!state?.running) return;
  profiles = await invoke("list_profiles");
  for (const sel of [/** @type {HTMLSelectElement} */ ($("#profile-pick")), /** @type {HTMLSelectElement} */ ($("#session-profile"))]) {
    const cur = sel.value;
    sel.replaceChildren(...profiles.map((p) => new Option(p.name, p.id)));
    if (profiles.some((p) => p.id === cur)) sel.value = cur;
  }
  await showProfile();
}

async function showProfile() {
  const id = /** @type {HTMLSelectElement} */ ($("#profile-pick")).value;
  const p = profiles.find((x) => x.id === id) ?? { id: "", name: "", instructions: "" };
  const f = form("#profile");
  for (const k of ["id", "name", "instructions"]) setField(f, k, p[k]);
  const ul = $("#docs");
  ul.replaceChildren();
  if (!id) return;
  for (const d of await invoke("list_docs", { profile: id })) {
    const li = document.createElement("li");
    const t = document.createElement("span");
    t.style.flex = "1";
    t.textContent = `${d.name} · ${d.chunks} chunks`;
    const del = document.createElement("button");
    del.className = "danger";
    del.textContent = "Delete";
    del.onclick = async () => {
      await call("delete_doc", { id: d.id });
      await showProfile();
    };
    li.append(t, del);
    ul.append(li);
  }
}

async function renderAbout() {
  if (!state?.running) return;
  const a = await invoke("get_about_me");
  const f = form("#about");
  for (const k of ["name", "role", "company", "product", "tone", "preferred_answers"]) setField(f, k, a?.[k]);
}

async function refreshDebug() {
  const rows = await invoke("debug_stats");
  const body = /** @type {HTMLElement} */ ($("#debug tbody"));
  body.replaceChildren(
    ...rows.map((/** @type {any} */ r) => {
      const tr = document.createElement("tr");
      for (const v of [r.from, r.to, r.count, r.p50_ms ?? "–", r.p95_ms ?? "–"]) {
        const td = document.createElement("td");
        td.textContent = String(v);
        tr.append(td);
      }
      return tr;
    }),
  );
}

async function refreshAll() {
  await refreshState();
  renderSettings();
  renderKeys();
  await renderChecklist();
  await renderProfiles();
  await renderAbout();
}

// ---------- actions ----------

$("#capture").onclick = async () => {
  await call("toggle_capture", { profile: /** @type {HTMLSelectElement} */ ($("#session-profile")).value || null });
  await refreshState();
};
$("#answer").onclick = () => call("answer_now");
$("#pair").onclick = async () => {
  const svg = await call("begin_pairing");
  const box = $("#qr");
  box.innerHTML = svg; // generated by ssc-crypto from the QR payload, not user content
  box.hidden = false;
  setTimeout(() => (box.hidden = true), 120_000); // the offer expires after 120 s
};
$("#unpair").onclick = async () => {
  if (!confirm("Unpair the phone? It will need to scan a new QR code.")) return;
  await call("unpair");
  await refreshState();
};
form("#about").onsubmit = async (e) => {
  e.preventDefault();
  const f = form("#about");
  const about = Object.fromEntries(["name", "role", "company", "product", "tone", "preferred_answers"].map((k) => [k, getField(f, k)]));
  await call("set_about_me", { about });
};
$("#profile-pick").onchange = showProfile;
$("#profile-new").onclick = () => {
  /** @type {HTMLSelectElement} */ ($("#profile-pick")).value = "";
  const f = form("#profile");
  for (const k of ["id", "name", "instructions"]) setField(f, k, "");
  $("#docs").replaceChildren();
};
form("#profile").onsubmit = async (e) => {
  e.preventDefault();
  const f = form("#profile");
  const profile = { id: getField(f, "id"), name: getField(f, "name"), instructions: getField(f, "instructions") };
  await call("put_profile", { profile });
  await renderProfiles();
  /** @type {HTMLSelectElement} */ ($("#profile-pick")).value = String(profile.id);
  await showProfile();
};
$("#doc-file").onchange = async (e) => {
  const input = /** @type {HTMLInputElement} */ (e.target);
  const profile = /** @type {HTMLSelectElement} */ ($("#profile-pick")).value;
  if (!profile) return showError("Save or pick a profile first.");
  for (const file of Array.from(input.files ?? [])) {
    $("#doc-status").textContent = `Indexing ${file.name}…`;
    const bytes = Array.from(new Uint8Array(await file.arrayBuffer()));
    const n = await call("upload_doc", { profile, name: file.name, bytes }).catch(() => null);
    $("#doc-status").textContent = n === null ? "" : `${file.name}: ${n} chunks`;
  }
  input.value = "";
  await showProfile();
};
form("#settings").onsubmit = async (e) => {
  e.preventDefault();
  const f = form("#settings");
  const c = structuredClone(state.config);
  for (const k of ["mode", "redaction", "redact_emails", "keep_transcripts", "testing"]) c[k] = getField(f, k);
  for (const k of ["relay_url", "lan_domain"]) c[k] = getField(f, k) || null;
  c.lan_port = Number(getField(f, "lan_port"));
  c.models.gemini_tier = getField(f, "gemini_tier");
  c.models.strong_model = getField(f, "strong_model") || null;
  c.models.strong_base_url = getField(f, "strong_base_url") || null;
  c.models.decision_cloud_base_url = getField(f, "decision_cloud_base_url");
  await call("save_config", { config: c });
  await refreshAll();
};

await refreshAll().catch(showError);
setInterval(() => refreshState().catch(() => {}), 2000);
setInterval(() => refreshDebug().catch(() => {}), 2000);
setInterval(() => renderChecklist().catch(() => {}), 5000);
