const pill = document.getElementById("pill");
const leading = document.getElementById("leading");
const bars = document.getElementById("bars");
const label = document.getElementById("label");

const BAR_COUNT = 24;
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
      label.textContent = "";
      renderLevel(payload.level ?? 0);
      break;
    case "transcribing":
      show();
      setLeading("transcribing");
      bars.style.display = "none";
      label.textContent = "Transcribing…";
      break;
    case "done":
      show();
      setLeading("done");
      bars.style.display = "none";
      label.textContent = "Pasted";
      break;
    case "message":
      show();
      setLeading("");
      bars.style.display = "none";
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
