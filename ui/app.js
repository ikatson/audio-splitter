const { invoke } = window.__TAURI__.core;

const STORAGE_KEY = "selection";
const VIRTUAL_DEVICE = /virtual audio cable|cable input/i;
const POLL_MS = 250;
const GAINS_KEY = "gains";
const MUTE_DB = -40;

const el = (id) => document.getElementById(id);
const selects = { source: el("source"), out1: el("out1"), out2: el("out2") };
const toggle = el("toggle");
const message = el("message");
let running = false;

function loadSelection() {
  try {
    return JSON.parse(localStorage.getItem(STORAGE_KEY)) || {};
  } catch {
    return {};
  }
}

function saveSelection() {
  try {
    localStorage.setItem(STORAGE_KEY, JSON.stringify({
      source: selects.source.value,
      out1: selects.out1.value,
      out2: selects.out2.value,
    }));
  } catch {}
}

// Slider positions in dB, remembered per device.
function loadGains() {
  try {
    return JSON.parse(localStorage.getItem(GAINS_KEY)) || {};
  } catch {
    return {};
  }
}

function saveGain(deviceId, db) {
  gains[deviceId] = db;
  try {
    localStorage.setItem(GAINS_KEY, JSON.stringify(gains));
  } catch {}
}

const gains = loadGains();
const linearGain = (db) => (db <= MUTE_DB ? 0 : 10 ** (db / 20));
const deviceDb = (deviceId) => gains[deviceId] ?? 0;

function showGain(slot) {
  const deviceId = selects[slot].value;
  const slider = el(`${slot}-gain`);
  slider.disabled = !deviceId;
  slider.value = deviceDb(deviceId);
  const db = Number(slider.value);
  el(`${slot}-gain-label`).textContent =
    db <= MUTE_DB ? "Muted" : `${db > 0 ? "+" : ""}${db} dB`;
}

function fillSelect(select, devices, allowNone, wanted) {
  select.replaceChildren();
  if (allowNone) select.add(new Option("None", ""));
  for (const d of devices) {
    select.add(new Option(d.is_default ? `${d.name} (Windows default)` : d.name, d.id));
  }
  if (devices.some((d) => d.id === wanted)) select.value = wanted;
}

async function refreshDevices() {
  const { outputs } = await invoke("list_devices");
  const saved = loadSelection();
  const current = {
    source: selects.source.value || saved.source,
    out1: selects.out1.value || saved.out1,
    out2: selects.out2.value || saved.out2,
  };
  const fallback = outputs.find((d) => VIRTUAL_DEVICE.test(d.name)) || outputs.find((d) => d.is_default);
  const source = outputs.some((d) => d.id === current.source) ? current.source : fallback?.id;
  fillSelect(selects.source, outputs, false, source);
  fillSelect(selects.out1, outputs, true, current.out1);
  fillSelect(selects.out2, outputs, true, current.out2);
  showGain("out1");
  showGain("out2");
}

function showMessage(text, isError = false) {
  message.textContent = text;
  message.classList.toggle("error", isError);
}

function setRunning(value) {
  running = value;
  toggle.textContent = value ? "Stop" : "Start";
  if (!value) {
    for (const id of ["out1", "out2"]) setInfo(id, "");
  }
}

async function start() {
  const outputs = [selects.out1.value, selects.out2.value]
    .filter(Boolean)
    .map((id) => ({ id, gain: linearGain(deviceDb(id)) }));
  try {
    await invoke("start", { source: selects.source.value, outputs });
    setRunning(true);
    showMessage("Running.");
  } catch (error) {
    setRunning(false);
    showMessage(String(error), true);
  }
}

async function stop() {
  await invoke("stop");
  setRunning(false);
  showMessage("Stopped.");
}

function setInfo(id, text, isError = false) {
  const info = el(`${id}-info`);
  info.textContent = text;
  info.classList.toggle("error", isError);
}

function describe(output) {
  if (output.error) return output.error;
  const parts = [`${output.sample_rate / 1000} kHz`, `${output.channels} ch`, `${Math.round(output.latency_ms)} ms buffer`];
  if (output.glitches) parts.push(`${output.glitches} glitches`);
  if (output.dropped_frames) parts.push(`${output.dropped_frames} frames dropped`);
  return parts.join(" · ");
}

async function poll() {
  if (!running) return;
  const status = await invoke("status");
  if (!status) return;
  if (status.source.error) showMessage(status.source.error, true);
  const ids = [selects.out1.value && "out1", selects.out2.value && "out2"].filter(Boolean);
  ids.forEach((id, i) => {
    const output = status.outputs[i];
    if (!output) return;
    setInfo(id, describe(output), Boolean(output.error));
  });
}

toggle.addEventListener("click", () => (running ? stop() : start()));
el("refresh").addEventListener("click", () => refreshDevices().catch((e) => showMessage(String(e), true)));
for (const select of Object.values(selects)) {
  select.addEventListener("change", () => {
    saveSelection();
    showGain("out1");
    showGain("out2");
    if (!running) return;
    if (selects.out1.value || selects.out2.value) {
      start();
    } else {
      stop();
    }
  });
}

for (const slot of ["out1", "out2"]) {
  el(`${slot}-gain`).addEventListener("input", (event) => {
    const deviceId = selects[slot].value;
    const db = Number(event.target.value);
    saveGain(deviceId, db);
    showGain(slot);
    if (running) invoke("set_gain", { id: deviceId, gain: linearGain(db) }).catch(() => {});
  });
}

refreshDevices()
  .then(() => {
    saveSelection();
    if (selects.out1.value || selects.out2.value) return start();
  })
  .catch((e) => showMessage(String(e), true));
setInterval(() => poll().catch(() => {}), POLL_MS);
startSpectrum(el("spectrum"), () => (running ? invoke("spectrum") : Promise.resolve(null)));
