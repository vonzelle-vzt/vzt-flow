const pill = document.getElementById("pill");
const leading = document.getElementById("leading");
const bars = document.getElementById("bars");
const label = document.getElementById("label");

// Reduced from 24: the waveform previously claimed the pill's full flexible
// width via #bars's `flex: 1`, leaving no room for the new elapsed-time
// label and truncating it to a couple characters (observed live: "0:12"
// rendered as "0…"). Fewer, still-flexible bars leaves guaranteed space for
// the mm:ss text (see overlay.html's #label/#bars width fix too).
const BAR_COUNT = 14;
for (let i = 0; i < BAR_COUNT; i++) {
  const bar = document.createElement("span");
  bar.style.height = "4px";
  bars.appendChild(bar);
}
const barEls = Array.from(bars.children);
// Rolling history of levels so the bars read left-to-right like a
// scrolling waveform instead of one static spike.
let history = new Array(BAR_COUNT).fill(0);

function renderLevel(level) {
  history.push(Math.max(0, Math.min(1, level)));
  history.shift();
  barEls.forEach((el, i) => {
    const h = 4 + history[i] * 24;
    el.style.height = `${h}px`;
  });
}

// mm:ss readout for the elapsed-time display during long-form holds (up to
// 10min) so the user has feedback during a multi-minute recording instead
// of a silent pill.
function formatElapsed(secs) {
  const whole = Math.max(0, Math.floor(secs));
  const m = Math.floor(whole / 60);
  const s = whole % 60;
  return `${m}:${String(s).padStart(2, "0")}`;
}

function setLeading(kind) {
  if (kind === "recording") {
    leading.innerHTML = '<div class="dot"></div>';
  } else if (kind === "transcribing") {
    leading.innerHTML = '<div class="spinner"></div>';
  } else if (kind === "done") {
    leading.innerHTML =
      '<svg class="check" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5"><path d="M4 12l5 5L20 6" stroke-linecap="round" stroke-linejoin="round"/></svg>';
  } else {
    leading.innerHTML = "";
  }
}

function show() { pill.classList.remove("hidden"); }
function hide() { pill.classList.add("hidden"); }

const { event } = window.__TAURI__;
event.listen("overlay://state", (e) => {
  const payload = e.payload;
  switch (payload.kind) {
    case "hidden":
      hide();
      break;
    case "recording":
      show();
      setLeading("recording");
      bars.style.display = "flex";
      label.textContent = formatElapsed(payload.elapsed_secs ?? 0);
      pill.classList.toggle("warning", !!payload.warning);
      renderLevel(payload.level ?? 0);
      break;
    case "transcribing":
      show();
      setLeading("transcribing");
      bars.style.display = "none";
      pill.classList.remove("warning");
      label.textContent = payload.mode ? `Transcribing… (${payload.mode})` : "Transcribing…";
      break;
    case "done":
      show();
      setLeading("done");
      bars.style.display = "none";
      pill.classList.remove("warning");
      label.textContent = "Pasted";
      break;
    case "message":
      show();
      setLeading("");
      bars.style.display = "none";
      pill.classList.remove("warning");
      label.textContent = payload.text ?? "";
      break;
  }
});

// Reset the waveform history whenever a fresh recording begins so the
// bars don't carry over a stale tail from the previous dictation.
let lastKind = null;
event.listen("overlay://state", (e) => {
  if (e.payload.kind === "recording" && lastKind !== "recording") {
    history = new Array(BAR_COUNT).fill(0);
  }
  lastKind = e.payload.kind;
});
