const { invoke } = window.__TAURI__.core;

const dotMic = document.getElementById("dot-mic");
const dotAx = document.getElementById("dot-ax");
const dotHotkey = document.getElementById("dot-hotkey");
const hotkeyHint = document.getElementById("hotkey-hint");
const btnMic = document.getElementById("btn-mic");
const btnAx = document.getElementById("btn-ax");
const btnHotkey = document.getElementById("btn-hotkey");
const hotkeySelect = document.getElementById("hotkey-select");
const holdThreshold = document.getElementById("hold-threshold");
const launchAtLogin = document.getElementById("launch-at-login");
const transcriptEl = document.getElementById("transcript");
const btnCopy = document.getElementById("btn-copy");
const profilesPathEl = document.getElementById("profiles-path");
const historyFilter = document.getElementById("history-filter");
const historyList = document.getElementById("history-list");
const dotModel = document.getElementById("dot-model");
const dotCleanup = document.getElementById("dot-cleanup");
const btnModel = document.getElementById("btn-model");
const btnCleanup = document.getElementById("btn-cleanup");
const modelProgressRow = document.getElementById("model-progress-row");
const modelProgress = document.getElementById("model-progress");

function setDot(el, ok) {
  el.classList.toggle("ok", ok);
  el.classList.toggle("bad", !ok);
}

async function refreshPermissions() {
  try {
    const status = await invoke("get_permission_status");
    setDot(dotMic, status.microphone_reachable);
    setDot(dotAx, status.accessibility_trusted);
    // Green exactly when the tap is armed. The re-arm driver flips this live
    // after a late grant, so no "restart after granting" note is needed. Show
    // a transient hint for the brief window where the grant has landed but the
    // tap hasn't armed yet, so a granted user isn't left staring at a red dot.
    setDot(dotHotkey, status.hotkey_monitor_active);
    if (status.hotkey_monitor_active) {
      hotkeyHint.textContent = "";
    } else if (status.input_monitoring_trusted) {
      hotkeyHint.textContent = "granted — arming…";
    } else {
      hotkeyHint.textContent = "";
    }
  } catch (e) {
    console.error("permission status failed", e);
  }
}

// The model status is polled slowly when idle and quickly while a download is
// in flight, so the percentage updates smoothly without hammering the backend
// (which stats the model dir on every call) the rest of the time.
let modelPollTimer = null;
let modelPollFast = false;

function scheduleModelPoll(fast) {
  if (modelPollTimer !== null && fast === modelPollFast) return;
  if (modelPollTimer !== null) clearInterval(modelPollTimer);
  modelPollFast = fast;
  modelPollTimer = setInterval(refreshModelStatus, fast ? 600 : 2500);
}

function phaseText(s) {
  switch (s.phase) {
    case "downloading":
      if (s.total > 0) {
        const pct = Math.round((s.downloaded / s.total) * 100);
        const mb = (s.downloaded / 1e6).toFixed(0);
        const totalMb = (s.total / 1e6).toFixed(0);
        return `Downloading… ${pct}% (${mb}/${totalMb} MB)`;
      }
      return "Downloading…";
    case "verifying":
      return "Verifying & installing…";
    case "extracting":
      return "Extracting…";
    case "error":
      return "Error: " + (s.error || "download failed");
    default:
      return "";
  }
}

function updateModelButton(btn, dot, present, active, kind) {
  setDot(dot, present);
  if (present) {
    btn.textContent = "Installed";
    btn.disabled = true;
  } else {
    btn.textContent = "Download";
    // Disable both download buttons while any download is running — the
    // backend serves a single download slot and refuses a second one.
    btn.disabled = active;
  }
}

async function refreshModelStatus() {
  try {
    const s = await invoke("get_model_status");
    const active = s.phase === "downloading" || s.phase === "verifying" || s.phase === "extracting";

    updateModelButton(btnModel, dotModel, s.parakeet_present, active, "parakeet");
    updateModelButton(btnCleanup, dotCleanup, s.cleanup_present, active, "cleanup");

    const text = phaseText(s);
    if (text) {
      modelProgress.textContent = text;
      modelProgressRow.style.display = "";
    } else {
      modelProgressRow.style.display = "none";
    }

    scheduleModelPoll(active);
  } catch (e) {
    console.error("model status failed", e);
  }
}

btnModel.addEventListener("click", async () => {
  await startDownload("parakeet");
});
btnCleanup.addEventListener("click", async () => {
  await startDownload("cleanup");
});

async function startDownload(kind) {
  try {
    await invoke("start_model_download", { kind });
    // Show progress immediately and switch to fast polling without waiting for
    // the next slow tick.
    modelProgressRow.style.display = "";
    modelProgress.textContent = "Starting…";
    scheduleModelPoll(true);
    refreshModelStatus();
  } catch (e) {
    modelProgressRow.style.display = "";
    modelProgress.textContent = "Error: " + e;
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

btnHotkey.addEventListener("click", async () => {
  // Fire the native Input Monitoring prompt first (a one-click grant on first
  // run; a no-op once already decided), then open the pane as the fallback for
  // users who dismissed or previously denied it. The re-arm driver picks up the
  // grant within ~2s and the dot flips green with no restart.
  try {
    await invoke("request_input_monitoring");
  } catch (e) {
    console.error("request input monitoring failed", e);
  }
  await invoke("open_input_monitoring_settings");
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
refreshModelStatus();
scheduleModelPoll(false);
setInterval(refreshPermissions, 2000);
setInterval(loadTranscript, 2000);
setInterval(loadHistory, 3000);
