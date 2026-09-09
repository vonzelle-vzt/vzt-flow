"use strict";
const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
const el = Object.fromEntries([
  "title", "elapsed", "state", "interview", "tips", "tip-headline", "tip-bullets",
  "tip-freshness", "notes", "transcript", "save-status", "update-files",
  "operation-error", "files", "open-transcript", "open-pdf"
].map(id => [id, document.getElementById(id)]));
let current = null;
let syncing = false;
let buffered = [];
let ready = false;
let resyncRequested = false;
let syncError = "";
// Session objects isolate async replies and retain unsaved drafts across binds.
const sessions = new Map();
const message = error => typeof error === "string" ? error : (error?.message || String(error));
const kind = s => typeof s.state === "string" ? s.state.toLowerCase() : (s.state?.kind || "recording").toLowerCase();
function clock(seconds, hours = false) {
  const n = Math.max(0, Math.floor(Number(seconds) || 0));
  const pad = n => String(n).padStart(2, "0");
  return hours ? `${pad(Math.floor(n / 3600))}:${pad(Math.floor(n / 60) % 60)}:${pad(n % 60)}` : `${pad(Math.floor(n / 60))}:${pad(n % 60)}`;
}
function bind(payload) {
  if (!payload?.session_id) return;
  if (current?.id === payload.session_id) return;
  if (current?.dirty) void save(current);
  current = sessions.get(payload.session_id) || {
    id: payload.session_id, text: "", dirty: false, edit: 0, rev: 0, used: 0,
    saving: null, timer: null, retry: null, saveError: "", error: "",
    lines: new Map(), seq: -1, tipSeq: -1, tip: null, frozenElapsed: null,
    modeEdit: 0, state: { kind: "recording" }, transcript_path: null, pdf_path: null
  };
  sessions.set(current.id, current);
  current.title = payload.title || "Meeting notes";
  current.started = Number(payload.started_at_ms) || Date.now();
  current.interview = !!payload.interview;
  current.tip = null;
  current.tipSeq = -1;
  el.notes.value = current.text;
  render();
}
function status() {
  const s = current;
  let text = s ? "Saved" : "Waiting for a meeting";
  let css = "";
  if (s?.saveError) { text = `Couldn't save: ${s.saveError}`; css = "error"; }
  else if (s?.saving) text = "Saving…";
  else if (s?.dirty) text = "Unsaved changes";
  else if (s?.pdf_path && s.rev > s.used) { text = "Edited after the PDF was written"; css = "edited"; }
  el["save-status"].textContent = text;
  el["save-status"].className = css;
  el["update-files"].hidden = !(s && kind(s) === "completed" && (s.rev > s.used || s.dirty));
  el["update-files"].disabled = !!(s?.exporting || s?.saving);
  const error = s?.error || syncError;
  el["operation-error"].textContent = error;
  el["operation-error"].hidden = !error;
}
function tick() {
  if (!current) return;
  const s = current;
  const elapsed = Math.max(0, (Date.now() - s.started) / 1000);
  if (kind(s) === "recording") s.frozenElapsed = null;
  else if (s.frozenElapsed === null) s.frozenElapsed = elapsed;
  el.elapsed.textContent = clock(s.frozenElapsed ?? elapsed);
  // Derive age from question time, not receipt time, so reopened stale tips
  // remain stale even though the frontend has just received its snapshot.
  el.tips.classList.toggle("stale", !!s.tip && Date.now() - (s.started + s.tip.question_offset_secs * 1000) > 45000);
}
function renderTip() {
  const s = current;
  el.tips.hidden = !s?.interview;
  const tip = s?.interview ? s.tip : null;
  el["tip-headline"].textContent = tip?.headline || "Tips will appear after a question.";
  el["tip-bullets"].replaceChildren();
  for (const bullet of (tip?.bullets || []).slice(0, 3)) {
    const li = document.createElement("li"); li.textContent = bullet; el["tip-bullets"].appendChild(li);
  }
  el["tip-freshness"].textContent = tip ? `for the question at ${clock(tip.question_offset_secs, true)} · ${(Math.max(0, Number(tip.latency_ms) || 0) / 1000).toFixed(1)}s` : "";
  tick();
}
function renderLines() {
  const lines = [...current.lines.values()].sort((a, b) => a.seq - b.seq);
  el.transcript.textContent = lines.length ? lines.map(line => `[${clock(line.offset_secs, true)}] ${line.source === "me" ? "Me" : "Them"}: ${line.text}`).join("\n") : "No transcript yet.";
}
function render() {
  const s = current;
  if (!s) { status(); return; }
  el.title.textContent = s.title;
  el.notes.disabled = false;
  el.interview.disabled = !!s.toggling;
  el.interview.checked = s.interview;
  const state = kind(s);
  el.state.dataset.kind = state;
  el.state.textContent = ({ recording: "Recording", stopping: "Finalizing…", finalizing: "Finalizing…", completed: "Saved", failed: "Failed" })[state] || "Waiting for a meeting";
  el.state.title = s.state?.step || "";
  el.files.hidden = state !== "completed";
  el["open-transcript"].hidden = !s.transcript_path;
  el["open-pdf"].hidden = !s.pdf_path;
  renderTip(); renderLines(); status();
}
function scheduleRetry(s) {
  clearTimeout(s.retry);
  s.retry = setTimeout(() => { s.retry = null; void save(s); }, 5000);
}
async function save(s = current) {
  if (!s || !s.dirty) return;
  clearTimeout(s.timer); s.timer = null;
  if (s.saving) return s.saving;
  const version = s.edit;
  const text = s.text;
  s.saving = (async () => {
    try {
      const rev = await invoke("save_meeting_notes", { sessionId: s.id, text });
      s.rev = Math.max(s.rev, Number(rev) || 0);
      s.dirty = s.edit !== version;
      s.saveError = "";
      clearTimeout(s.retry); s.retry = null;
    } catch (error) {
      s.saveError = message(error); s.dirty = true; scheduleRetry(s);
    } finally {
      s.saving = null;
      if (current === s) status();
      // Serialize autosaves: an older request can never overwrite a newer one.
      if (s.dirty && !s.saveError) s.timer = setTimeout(() => void save(s), 0);
    }
  })();
  if (current === s) status();
  return s.saving;
}
function apply(name, p, floor = -1) {
  if (name === "meeting://bind") { bind(p); return; }
  const s = current;
  if (!s || p?.session_id !== s.id) return;
  if (Number.isFinite(p.seq) && p.seq <= floor) return;
  switch (name) {
    case "meeting://line":
      if (!Number.isFinite(p.seq) || p.seq <= s.seq) return;
      s.lines.set(p.seq, p); s.seq = p.seq; renderLines(); break;
    case "meeting://tip":
      if (!s.interview || !Number.isFinite(p.seq) || p.seq <= s.tipSeq) return;
      s.tip = p; s.tipSeq = p.seq; renderTip(); break;
    case "meeting://state":
      s.state = p.state || s.state;
      for (const key of ["transcript_path", "pdf_path", "error"]) if (key in p) s[key] = p[key];
      // This event does not carry notes_rev_used. Fetch its authoritative value.
      if (kind(s) === "completed") void snapshot();
      render(); break;
    case "meeting://notes-status":
      s.rev = Math.max(s.rev, Number(p.rev) || 0);
      // Only the matching invoke result acknowledges our text revision.
      if (!p.ok) { s.saveError = message(p.error || "Unknown write error"); if (s.dirty) scheduleRetry(s); }
      status(); break;
  }
}
async function snapshot() {
  if (!ready) return;
  if (syncing) { resyncRequested = true; return; }
  syncing = true;
  const atStart = current;
  const editAtStart = current?.edit;
  const modeAtStart = current?.modeEdit;
  let floor = -1;
  try {
    const data = await invoke("get_meeting_snapshot");
    syncError = "";
    // Initial hydration is the only snapshot allowed to bind. Subsequent
    // session changes must arrive as meeting://bind, including during fetch.
    if (!current && data?.session_id) bind(data);
    const s = current;
    if (data?.session_id && s?.id === data.session_id && (!atStart || atStart === s)) {
      s.title = data.title || s.title;
      s.started = Number(data.started_at_ms) || s.started;
      s.state = data.state || s.state;
      s.rev = Math.max(s.rev, Number(data.notes_rev) || 0);
      s.used = Number(data.notes_rev_used) || 0;
      if (!s.toggling && (modeAtStart === undefined || modeAtStart === s.modeEdit)) s.interview = !!data.interview;
      s.transcript_path = data.transcript_path;
      s.pdf_path = data.pdf_path;
      s.error = data.error || "";
      floor = Number.isFinite(data.seq) ? data.seq : -1;
      for (const line of data.lines || []) if (line.session_id === s.id) s.lines.set(line.seq, line);
      s.seq = Math.max(s.seq, floor);
      if (!s.dirty && !s.saving && (Number(data.notes_rev) || 0) >= s.rev && (editAtStart === undefined || editAtStart === s.edit)) {
        s.text = data.notes || ""; el.notes.value = s.text;
      }
      s.tip = s.interview ? data.last_tip : null;
      s.tipSeq = s.tip?.seq ?? -1;
      render();
    }
  } catch (error) {
    syncError = `Couldn't refresh meeting: ${message(error)}`; status();
  } finally {
    syncing = false;
    const pending = buffered; buffered = [];
    for (const [name, payload] of pending) apply(name, payload, floor);
    if (resyncRequested) { resyncRequested = false; setTimeout(() => void snapshot(), 0); }
  }
}
el.notes.addEventListener("input", () => {
  if (!current) return;
  const s = current; s.text = el.notes.value; s.edit++; s.dirty = true;
  clearTimeout(s.timer); s.timer = setTimeout(() => void save(s), 500); status();
});
el.notes.addEventListener("blur", () => void save());
window.addEventListener("beforeunload", () => { for (const s of sessions.values()) if (s.dirty) void save(s); });
document.addEventListener("visibilitychange", () => {
  if (document.visibilityState === "visible") void snapshot();
  else void save();
});
el.interview.addEventListener("change", async () => {
  const s = current; if (!s || s.toggling) return;
  const previous = s.interview;
  s.interview = el.interview.checked; s.modeEdit++; s.toggling = true; s.tip = null; render();
  try { await invoke("set_interview_mode", { sessionId: s.id, on: s.interview }); s.error = ""; }
  catch (error) { s.interview = previous; s.error = `Couldn't change interview mode: ${message(error)}`; }
  finally { s.toggling = false; if (current === s) render(); }
});
el["update-files"].addEventListener("click", async () => {
  const s = current; if (!s || s.exporting) return;
  s.exporting = true; status();
  try {
    // Drain edits made while an earlier save was in flight before exporting.
    do { await save(s); if (s.saveError) throw new Error(s.saveError); } while (s.dirty);
    const exportedRev = s.rev;
    const path = await invoke("reexport_meeting", { sessionId: s.id });
    s.pdf_path = path; s.used = exportedRev; s.error = "";
  } catch (error) { s.error = `Couldn't update files: ${message(error)}`; }
  finally { s.exporting = false; if (current === s) render(); }
});
for (const [id, key] of [["open-transcript", "transcript_path"], ["open-pdf", "pdf_path"]]) {
  el[id].addEventListener("click", async () => {
    const s = current; if (!s?.[key]) return;
    try { await invoke("reveal_in_finder", { path: s[key] }); }
    catch (error) { if (current === s) { s.error = `Couldn't open file: ${message(error)}`; status(); } }
  });
}
async function start() {
  try {
    // Register every listener before asking for the snapshot. Bind is handled
    // immediately so an old in-flight snapshot cannot rebind the new session.
    syncing = true;
    for (const name of ["meeting://bind", "meeting://state", "meeting://line", "meeting://tip", "meeting://notes-status"]) {
      await listen(name, event => {
        if (name === "meeting://bind") { bind(event.payload); if (ready) void snapshot(); }
        else if (syncing) buffered.push([name, event.payload]);
        else apply(name, event.payload);
      });
    }
    ready = true; syncing = false; await snapshot();
  } catch (error) { syncError = `Couldn't connect to meeting: ${message(error)}`; status(); }
}
setInterval(tick, 1000);
void start();
