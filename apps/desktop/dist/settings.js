const { invoke } = window.__TAURI__.core;

const dotMic = document.getElementById("dot-mic");
const dotAx = document.getElementById("dot-ax");
const dotHotkey = document.getElementById("dot-hotkey");
const btnMic = document.getElementById("btn-mic");
const btnAx = document.getElementById("btn-ax");
const hotkeySelect = document.getElementById("hotkey-select");
const holdThreshold = document.getElementById("hold-threshold");
const launchAtLogin = document.getElementById("launch-at-login");
const transcriptEl = document.getElementById("transcript");
const btnCopy = document.getElementById("btn-copy");
const profilesPathEl = document.getElementById("profiles-path");
const historyFilter = document.getElementById("history-filter");
const historyList = document.getElementById("history-list");

function setDot(el, ok) {
  el.classList.toggle("ok", ok);
  el.classList.toggle("bad", !ok);
}

async function refreshPermissions() {
  try {
    const status = await invoke("get_permission_status");
    setDot(dotMic, status.microphone_reachable);
    setDot(dotAx, status.accessibility_trusted);
    setDot(dotHotkey, status.hotkey_monitor_active);
  } catch (e) {
    console.error("permission status failed", e);
  }
}

async function loadConfig() {
  const config = await invoke("get_config");
  hotkeySelect.value = String(config.hotkey_keycode);
  holdThreshold.value = config.hold_threshold_ms;
  launchAtLogin.checked = config.launch_at_login;
}

async function saveConfig() {
  const config = await invoke("get_config");
  config.hotkey_keycode = Number(hotkeySelect.value);
  config.hotkey_label = hotkeySelect.options[hotkeySelect.selectedIndex].text;
  config.hold_threshold_ms = Number(holdThreshold.value);
  config.launch_at_login = launchAtLogin.checked;
  await invoke("set_config", { config });
}

async function loadTranscript() {
  const text = await invoke("get_last_transcript");
  transcriptEl.textContent = text || "(none yet)";
}

async function loadProfilesPath() {
  try {
    const path = await invoke("get_profiles_path");
    profilesPathEl.textContent = path || "(unavailable)";
  } catch (e) {
    profilesPathEl.textContent = "(unavailable)";
  }
}

let lastHistory = [];

function renderHistory() {
  const filter = historyFilter.value.trim().toLowerCase();
  const rows = filter
    ? lastHistory.filter(
        (e) =>
          e.clean_text.toLowerCase().includes(filter) ||
          e.raw_text.toLowerCase().includes(filter) ||
          (e.app || "").toLowerCase().includes(filter) ||
          e.mode.toLowerCase().includes(filter)
      )
    : lastHistory;

  historyList.innerHTML = "";
  if (rows.length === 0) {
    const empty = document.createElement("div");
    empty.className = "history-empty";
    empty.textContent = lastHistory.length === 0 ? "(none yet)" : "(no matches)";
    historyList.appendChild(empty);
    return;
  }

  for (const entry of rows) {
    const row = document.createElement("div");
    row.className = "history-row";

    const meta = document.createElement("div");
    meta.className = "history-meta";
    const date = new Date(entry.ts * 1000);
    meta.textContent = `${date.toLocaleString()} · ${entry.mode}${entry.app ? " · " + entry.app : ""}`;

    const text = document.createElement("div");
    text.className = "history-text";
    text.textContent = entry.clean_text || entry.raw_text;

    row.appendChild(meta);
    row.appendChild(text);
    row.addEventListener("click", async () => {
      await invoke("copy_text", { text: entry.clean_text || entry.raw_text });
    });
    historyList.appendChild(row);
  }
}

async function loadHistory() {
  try {
    lastHistory = await invoke("get_history");
    renderHistory();
  } catch (e) {
    console.error("failed to load history", e);
  }
}

historyFilter.addEventListener("input", renderHistory);

btnMic.addEventListener("click", async () => {
  await invoke("get_permission_status"); // probe_microphone happens inside
  await refreshPermissions();
});

btnAx.addEventListener("click", async () => {
  await invoke("open_accessibility_settings");
});

hotkeySelect.addEventListener("change", saveConfig);
holdThreshold.addEventListener("change", saveConfig);
launchAtLogin.addEventListener("change", saveConfig);

btnCopy.addEventListener("click", async () => {
  await invoke("copy_last_transcript");
});

loadConfig();
loadTranscript();
loadProfilesPath();
loadHistory();
refreshPermissions();
setInterval(refreshPermissions, 2000);
setInterval(loadTranscript, 2000);
setInterval(loadHistory, 3000);
