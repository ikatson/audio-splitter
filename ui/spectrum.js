// Draws the band levels from the `spectrum` command as bars with falling peak caps.
function startSpectrum(canvas, readBands) {
  const BAND_COUNT = 64;
  // Must match MIN_HZ and MAX_HZ in spectrum.rs.
  const MIN_HZ = 30;
  const MAX_HZ = 18000;
  const LABELS = [[100, "100"], [1000, "1k"], [10000, "10k"]];
  const FALL_PER_S = 1.6;
  const PEAK_HOLD_S = 0.5;
  const PEAK_FALL_PER_S = 0.5;
  const AXIS_HEIGHT = 18;

  const ctx = canvas.getContext("2d");
  const levels = new Float32Array(BAND_COUNT);
  const peaks = new Float32Array(BAND_COUNT);
  const holds = new Float32Array(BAND_COUNT);
  let target = new Float32Array(BAND_COUNT);
  let lastTime = performance.now();
  let reading = false;

  function resize() {
    const ratio = window.devicePixelRatio || 1;
    canvas.width = Math.round(canvas.clientWidth * ratio);
    canvas.height = Math.round(canvas.clientHeight * ratio);
  }
  new ResizeObserver(resize).observe(canvas);

  function update(dt) {
    for (let i = 0; i < BAND_COUNT; i++) {
      // Jump up to catch transients, fall gradually so it doesn't flicker.
      levels[i] = Math.max(target[i], levels[i] - FALL_PER_S * dt);
      if (levels[i] >= peaks[i]) {
        peaks[i] = levels[i];
        holds[i] = PEAK_HOLD_S;
      } else if (holds[i] > 0) {
        holds[i] -= dt;
      } else {
        peaks[i] = Math.max(0, peaks[i] - PEAK_FALL_PER_S * dt);
      }
    }
  }

  function draw() {
    const ratio = window.devicePixelRatio || 1;
    const width = canvas.width / ratio;
    const height = canvas.height / ratio;
    const plotHeight = height - AXIS_HEIGHT;
    const style = getComputedStyle(document.documentElement);

    ctx.setTransform(ratio, 0, 0, ratio, 0, 0);
    ctx.clearRect(0, 0, width, height);

    const gradient = ctx.createLinearGradient(0, plotHeight, 0, 0);
    gradient.addColorStop(0, style.getPropertyValue("--viz-low"));
    gradient.addColorStop(0.6, style.getPropertyValue("--viz-mid"));
    gradient.addColorStop(1, style.getPropertyValue("--viz-high"));

    const slot = width / BAND_COUNT;
    const barWidth = Math.max(1, slot - Math.min(2, slot * 0.25));
    const minHeight = 2;
    for (let i = 0; i < BAND_COUNT; i++) {
      const x = i * slot + (slot - barWidth) / 2;
      const barHeight = Math.max(minHeight, levels[i] * plotHeight);
      ctx.fillStyle = levels[i] > 0 ? gradient : style.getPropertyValue("--track");
      ctx.fillRect(x, plotHeight - barHeight, barWidth, barHeight);
      if (peaks[i] > 0.01) {
        ctx.fillStyle = style.getPropertyValue("--viz-high");
        ctx.fillRect(x, plotHeight - peaks[i] * plotHeight - 2, barWidth, 2);
      }
    }

    ctx.fillStyle = style.getPropertyValue("--muted");
    ctx.font = "11px 'Segoe UI', system-ui, sans-serif";
    ctx.textAlign = "center";
    ctx.textBaseline = "bottom";
    for (const [hz, label] of LABELS) {
      const x = (Math.log(hz / MIN_HZ) / Math.log(MAX_HZ / MIN_HZ)) * width;
      ctx.fillText(label, x, height - 2);
    }
  }

  function frame(now) {
    const dt = Math.min(0.1, (now - lastTime) / 1000);
    lastTime = now;
    if (!reading) {
      reading = true;
      readBands()
        .then((bands) => {
          target = bands ? Float32Array.from(bands) : new Float32Array(BAND_COUNT);
        })
        .catch(() => {})
        .finally(() => {
          reading = false;
        });
    }
    update(dt);
    draw();
    requestAnimationFrame(frame);
  }

  resize();
  requestAnimationFrame(frame);
}
