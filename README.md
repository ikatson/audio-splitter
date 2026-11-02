# Audio Splitter

A small Windows app that plays your system sound on two output devices at once, such as a TV and a headset. Built with Rust and Tauri.

It captures everything playing on one output device and repeats it to up to two others in real time. It also shows a live frequency spectrum of the audio.

## Requirements

- Windows 10 or 11
- A virtual audio device for apps to play into, such as [VB-CABLE](https://vb-audio.com/Cable/), Virtual Audio Cable or Voicemeeter

The app doesn't create an output device itself, because Windows requires a signed kernel driver for that. It uses one you already have installed.

To build it, you also need Rust and the Tauri CLI:

```bash
cargo install tauri-cli --version "^2"
```

## Build

```bash
cd src-tauri
cargo tauri build --no-bundle
```

The app is built at `src-tauri/target/release/audio-splitter.exe`.

## Usage

1. Open the app and choose the virtual device at the top. A device with "Virtual Audio Cable" or "CABLE Input" in its name is selected automatically.
2. Choose **Output 1** and **Output 2**. Set one to **None** if you only need one.
3. Click **Start**.
4. In **Settings → System → Sound → Output**, select the virtual device.

To route only some apps, leave the Windows output alone and assign those apps to the virtual device under **Settings → System → Sound → Volume mixer**.

Your selections and volumes are remembered. If an output was selected last time, the app starts automatically on launch.

## Features

- **Volume per output:** from muted to +12 dB, remembered per device and adjustable during playback. Boosting can clip loud audio.
- **Mismatched devices:** handles different sample rates, channel counts and device clocks.
- **Spectrum display:** shows 30 Hz to 18 kHz.

## Limitations

- The app adds about 40 ms of delay, and more if it has to recover from glitches.
- The two outputs may be slightly out of sync, especially with wireless devices. There is no delay control yet.
- If an output disconnects, reconnect it, click **Refresh devices**, then **Stop** and **Start**.
- An output can't be the same device as the source, because that would feed back into itself.
- Routing a device back into the source through other software, such as Voicemeeter, can't be detected and will also cause feedback.

## Development

```bash
cd src-tauri
cargo test --lib
```

`examples/e2e.rs` checks the audio path on real devices. It plays a quiet test tone that can be audible, so choose outputs that aren't routed to speakers.
