const { invoke } = window.__TAURI__.core;

const DEVICES_KEY = "devices";
const GAINS_KEY = "gains";
const POLL_MS = 250;
const MPV_POLL_MS = 1000;
const MUTE_DB = -40;
const SLOTS = ["out1", "out2"];

const el = (id) => document.getElementById(id);
const source = el("source");
const toggle = el("toggle");
const message = el("message");
let devices = [];
let running = false;
// The slot each running engine output belongs to.
let outputSlots = [];

let mpvStatus = null;
// Track picked per device for the video mpv has open.
let trackChoice = {};
// Whether outputs hear their own tracks through hidden mpvs instead of a copy of the source.
let following = false;
let mpvRowsKey = "";

function load(key) {
  try {
    return JSON.parse(localStorage.getItem(key)) || {};
  } catch {
    return {};
  }
}

function save(key, value) {
  try {
    localStorage.setItem(key, JSON.stringify(value));
  } catch {}
}

function saveDevices() {
  save(DEVICES_KEY, { source: source.value, out1: el("out1").value, out2: el("out2").value });
}

// Slider positions in dB, remembered per device.
const gains = load(GAINS_KEY);
const linearGain = (db) => (db <= MUTE_DB ? 0 : 10 ** (db / 20));
const deviceDb = (deviceId) => gains[deviceId] ?? 0;
const outputFor = (id) => ({ id, gain: linearGain(deviceDb(id)) });
// For hidden mpvs: dB, or null to mute.
const followerGain = (deviceId) => (deviceDb(deviceId) <= MUTE_DB ? null : deviceDb(deviceId));
const deviceName = (deviceId) => devices.find((d) => d.id === deviceId)?.name;

function showGain(slot) {
  const deviceId = el(slot).value;
  const slider = el(`${slot}-gain`);
  slider.disabled = !deviceId;
  slider.value = deviceDb(deviceId);
  const db = Number(slider.value);
  el(`${slot}-gain-label`).textContent = db <= MUTE_DB ? "Muted" : `${db > 0 ? "+" : ""}${db} dB`;
}

// Playing a device into itself would feed back, and one device can't be both outputs.
function excludeConflicts() {
  for (const slot of SLOTS) {
    const select = el(slot);
    const other = el(SLOTS.find((s) => s !== slot)).value;
    for (const option of select.options) {
      option.disabled = option.value !== "" && (option.value === source.value || option.value === other);
    }
    if (select.selectedOptions[0]?.disabled) select.value = "";
  }
}

function fillSelect(select, list, allowNone, wanted) {
  select.replaceChildren();
  if (allowNone) select.add(new Option("None", ""));
  for (const d of list) {
    select.add(new Option(d.is_default ? `${d.name} (Windows default)` : d.name, d.id));
  }
  if (list.some((d) => d.id === wanted)) select.value = wanted;
}

async function refreshDevices() {
  devices = (await invoke("list_devices")).outputs;
  const saved = load(DEVICES_KEY);
  const wantedSource = source.value || saved.source;
  const fallback = devices.find((d) => d.is_default);
  fillSelect(source, devices, false, devices.some((d) => d.id === wantedSource) ? wantedSource : fallback?.id);
  for (const slot of SLOTS) {
    // Versions with a single "duplicate" output saved it under that name.
    const savedOutput = saved[slot] ?? (slot === "out2" ? saved.duplicate : undefined);
    fillSelect(el(slot), devices, true, el(slot).value || savedOutput);
  }
  excludeConflicts();
  SLOTS.forEach(showGain);
  renderMpv();
}

function showMessage(text, isError = false) {
  message.textContent = text;
  message.classList.toggle("error", isError);
}

function setInfo(slot, text, isError = false) {
  const info = el(`${slot}-info`);
  info.textContent = text;
  info.classList.toggle("error", isError);
}

function setRunning(value) {
  running = value;
  toggle.textContent = value ? "Stop" : "Start";
  if (!value) SLOTS.forEach((slot) => setInfo(slot, ""));
}

const activeSlots = () => SLOTS.filter((slot) => el(slot).value);

// The track a device should hear: its pick, else what mpv plays, else the first track.
const chosenTrack = (deviceId) => trackChoice[deviceId] ?? mpvStatus.selected ?? mpvStatus.tracks[0]?.id ?? null;

// mpv plays on the source; each output either copies it or follows with its own track.
function trackPlan() {
  if (!mpvStatus || !source.value) return null;
  const sourceTrack = chosenTrack(source.value);
  const outputs = activeSlots().map((slot) => ({ slot, id: el(slot).value, track: chosenTrack(el(slot).value) }));
  return { sourceTrack, outputs, differs: outputs.some((o) => o.track !== sourceTrack) };
}

async function stopFollowing() {
  if (!following) return;
  await invoke("follow_stop");
  following = false;
}

async function startCopying(plan) {
  await stopFollowing();
  if (plan && plan.sourceTrack !== mpvStatus.selected) await invoke("mpv_select_track", { id: plan.sourceTrack });
  const slots = activeSlots();
  if (slots.length === 0) throw "Choose at least one output.";
  await invoke("start", { source: source.value, outputs: slots.map((slot) => outputFor(el(slot).value)) });
  outputSlots = slots;
}

async function startFollowing(plan) {
  // The outputs get their sound from hidden mpvs, so copying the source would double it.
  await invoke("stop");
  if (plan.sourceTrack !== mpvStatus.selected) await invoke("mpv_select_track", { id: plan.sourceTrack });
  const followers = plan.outputs.map((o) => ({ device: deviceName(o.id), track: o.track, gain_db: followerGain(o.id) }));
  await invoke("follow_start", { followers });
  following = true;
  outputSlots = plan.outputs.map((o) => o.slot);
}

async function start() {
  const plan = trackPlan();
  try {
    if (plan?.differs) {
      await startFollowing(plan);
    } else {
      await startCopying(plan);
    }
    setRunning(true);
    showMessage(following ? "Running, with each listener's own track." : "Running.");
  } catch (error) {
    await invoke("stop");
    await stopFollowing().catch(() => {});
    setRunning(false);
    showMessage(String(error), true);
  }
}

async function stop() {
  await invoke("stop");
  await stopFollowing().catch(() => {});
  setRunning(false);
  showMessage("Stopped.");
}

function describe(output) {
  if (output.error) return output.error;
  const parts = [`${output.sample_rate / 1000} kHz`, `${output.channels} ch`, `${Math.round(output.latency_ms)} ms buffer`];
  if (output.glitches) parts.push(`${output.glitches} glitches`);
  if (output.dropped_frames) parts.push(`${output.dropped_frames} frames dropped`);
  return parts.join(" · ");
}

function describeFollower(follower) {
  if (follower.error) return follower.error;
  if (follower.offset_ms == null) return "Starting…";
  return `Own track · ${Math.abs(Math.round(follower.offset_ms))} ms from the video`;
}

async function poll() {
  if (!running) return;
  if (following) {
    const followers = (await invoke("follow_status")) ?? [];
    outputSlots.forEach((slot, i) => {
      if (followers[i]) setInfo(slot, describeFollower(followers[i]), Boolean(followers[i].error));
    });
    return;
  }
  const status = await invoke("status");
  if (!status) return;
  if (status.source.error) showMessage(status.source.error, true);
  outputSlots.forEach((slot, i) => {
    const output = status.outputs[i];
    if (output) setInfo(slot, describe(output), Boolean(output.error));
  });
}

function renderMpv() {
  const panel = el("mpv");
  panel.hidden = !mpvStatus;
  if (!mpvStatus) return;
  el("mpv-title").textContent = mpvStatus.title ? `· ${mpvStatus.title}` : "";

  const people = [source.value, ...activeSlots().map((slot) => el(slot).value)]
    .map((id) => devices.find((d) => d.id === id))
    .filter(Boolean);
  const key = JSON.stringify([people.map((d) => d.id), mpvStatus.tracks]);
  if (key !== mpvRowsKey) {
    mpvRowsKey = key;
    el("mpv-rows").replaceChildren(
      ...people.map((device) => {
        const row = document.createElement("div");
        row.className = "mpv-row";
        const label = document.createElement("label");
        label.textContent = device.name;
        label.htmlFor = `track-${device.id}`;
        const select = document.createElement("select");
        select.id = `track-${device.id}`;
        for (const track of mpvStatus.tracks) select.add(new Option(track.label, track.id));
        select.addEventListener("change", () => {
          trackChoice[device.id] = Number(select.value);
          start();
        });
        row.append(label, select);
        return row;
      }),
    );
  }
  for (const device of people) {
    const select = el(`track-${device.id}`);
    const wanted = chosenTrack(device.id);
    if (select && wanted != null && document.activeElement !== select) select.value = wanted;
  }

  let note = "";
  if (activeSlots().length === 0) note = "Choose the other headphones as an output above.";
  else if (mpvStatus.tracks.length < 2) note = "This video has one audio track.";
  el("mpv-note").textContent = note;
}

async function pollMpv() {
  const status = await invoke("mpv_status");
  const fileChanged = (status?.path ?? null) !== (mpvStatus?.path ?? null);
  mpvStatus = status;
  if (fileChanged) {
    trackChoice = {};
    // Hidden mpvs play the old file, or close with mpv; go back to copying the source.
    if (running && following) start();
  }
  renderMpv();
}

function restartIfRunning() {
  if (!running) return;
  if (activeSlots().length > 0) {
    start();
  } else {
    stop();
  }
}

toggle.addEventListener("click", () => (running ? stop() : start()));
el("refresh").addEventListener("click", () => refreshDevices().catch((e) => showMessage(String(e), true)));
source.addEventListener("change", () => {
  excludeConflicts();
  SLOTS.forEach(showGain);
  saveDevices();
  renderMpv();
  restartIfRunning();
});
for (const slot of SLOTS) {
  el(slot).addEventListener("change", () => {
    excludeConflicts();
    showGain(slot);
    saveDevices();
    renderMpv();
    restartIfRunning();
  });
  el(`${slot}-gain`).addEventListener("input", (event) => {
    const deviceId = el(slot).value;
    const db = Number(event.target.value);
    gains[deviceId] = db;
    save(GAINS_KEY, gains);
    showGain(slot);
    if (!running) return;
    if (following) {
      invoke("follow_set_gain", { device: deviceName(deviceId), gainDb: followerGain(deviceId) }).catch(() => {});
    } else {
      invoke("set_gain", { id: deviceId, gain: linearGain(db) }).catch(() => {});
    }
  });
}

refreshDevices()
  .then(() => pollMpv().catch(() => {}))
  .then(() => {
    saveDevices();
    if (activeSlots().length > 0) return start();
  })
  .catch((e) => showMessage(String(e), true));
setInterval(() => poll().catch(() => {}), POLL_MS);
setInterval(() => pollMpv().catch(() => {}), MPV_POLL_MS);
startSpectrum(el("spectrum"), () => (running && !following ? invoke("spectrum") : Promise.resolve(null)));
