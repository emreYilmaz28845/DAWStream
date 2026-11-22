<h1 align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/wordmark-dark.svg">
    <img src="assets/wordmark-light.svg" alt="DAWStream" width="480">
  </picture>
</h1>

<p align="center">Share your DAW on Discord <b>with sound</b>.</p>

If you've ever tried to screen-share FL Studio, Ableton, Cubase etc. on Discord, you know the problem: your friends see the screen but hear nothing. That's because your DAW plays audio through ASIO, and Discord can't pick up ASIO audio.

The usual fix is a pile of routing software (Voicemeeter, VB-Cable, OBS…). DAWStream replaces all of that with one small plugin and one small app.

> Windows 10/11, 64-bit only.

## How it works

DAWStream has two parts:

- **The plugin** (VST3 / CLAP). You put it on your Master channel. It doesn't change your sound at all — it just quietly sends a copy of it to the app.
- **The app** (`DAWStream.exe`). It shows a live mirror of your DAW window and plays the audio it gets from the plugin through a normal Windows output device.

You then share the **DAWStream window** in Discord instead of the DAW itself. Discord sees a regular app with regular Windows audio, so the sound goes through.

You keep listening through your audio interface exactly like before. The app plays the copy on a *different* output that you're not listening to (for example your monitor's HDMI audio or an unused S/PDIF port), so you don't hear everything twice.

## Install

1. Download `DAWStream-vX.Y.Z-windows-x64.zip` from the [Releases](../../releases) page and unzip it.
2. Double-click **`Install.bat`**.

   This puts the app and the plugin into your user folder and creates a desktop shortcut.

   **Run it as administrator instead** (right-click → *Run as administrator*) if either of these applies to you:
   - Your DAW only looks for plugins in `C:\Program Files\Common Files\VST3`
   - You run your DAW as administrator

   Where things end up:

   | | Normal install | Admin install |
   |---|---|---|
   | App | `%LOCALAPPDATA%\DAWStream` | `C:\Program Files\DAWStream` |
   | VST3 | `%LOCALAPPDATA%\Programs\Common\VST3` | `C:\Program Files\Common Files\VST3` |
   | CLAP | `%LOCALAPPDATA%\Programs\Common\CLAP` | `C:\Program Files\Common Files\CLAP` |

3. Open your DAW and rescan plugins.

> Windows may show a SmartScreen warning because the files aren't code-signed. They're built by GitHub Actions straight from this repo (see [`.github/workflows/release.yml`](.github/workflows/release.yml)), and every release has a `SHA256SUMS.txt` you can check against.

## Using it

**In your DAW**

1. Add **DAWStream** as the **last effect on your Master channel**. Anything after it won't be heard on the stream.
2. Open the plugin window and click **Open App** (or open DAWStream from the desktop shortcut).

Tip: put it in your default project template so you never have to think about it again. When the app isn't running, the plugin does basically nothing.

**In the DAWStream app**

1. **Window:** pick your DAW. It's usually selected for you (marked `[DAW]`). Hit *Refresh* if you opened the DAW after the app.
2. **Audio Output:** pick a device you're *not* listening on. Devices that are probably safe are marked `[Recommended]`. If you pick your main audio interface you'll get a `[!] Double audio risk` warning — you'll hear yourself twice.
3. The dot in the top-left should turn green and say **LIVE**. Play something and the meter should move.

**In Discord**

1. Click **Share Screen → Applications** and pick **DAWStream — Discord DAW Streaming**.
2. Turn on sound sharing if Discord asks.
3. Press **H** in DAWStream to hide the toolbar so your viewers only see the DAW. Press **H** again (or move the mouse to the top edge) to bring it back.

### Controls

| Where | Control | What it does |
|---|---|---|
| Plugin | **AUDIO / MUTED** | Mutes the stream only. You still hear your DAW normally. |
| Plugin | **Gain slider** (-60 to +12 dB) | Makes the stream louder or quieter for your listeners. Doesn't touch your own mix. |
| Plugin | **Take Over** | Shows up if DAWStream is on more than one track/project. Click it on the instance that should be streamed. |
| App | **AUDIO / MUTED** + volume (0–150%) | Same idea, on the app side. |
| App | **H** / *Hide Bar* | Hides the toolbar for a clean stream. |

The app remembers your audio output, volume and mute setting between sessions.

## Supported DAWs

Anything 64-bit on Windows that can load **VST3** or **CLAP** plugins should work. The app recognizes these automatically:

FL Studio, Ableton Live, Cubase, Nuendo, REAPER, Studio One, Bitwig Studio, Reason, Cakewalk, Waveform, Mixcraft, Renoise, Ardour, LMMS, and others.

If your DAW isn't auto-detected, just pick its window by hand from the list.

**Pro Tools** isn't supported for audio — it only loads AAX plugins, not VST3/CLAP.

## Troubleshooting

**I hear everything twice / there's an echo.**
The app's Audio Output is set to the same device you listen on. Pick a different one — an unused HDMI/DisplayPort monitor output, `Realtek Digital Output`, or anything else you don't have headphones plugged into.

**I don't have any spare output device.**
Install a free virtual audio device such as [VB-Cable](https://vb-audio.com/Cable/) and select **CABLE Input** as the Audio Output. No extra routing needed — just select it.

**The status says "Waiting for Plugin".**
The plugin isn't loaded, or audio isn't running in your DAW. Make sure DAWStream is on the Master channel and your DAW's audio engine is running (check the DAW's audio settings if it's set to release the audio device in the background).

**The plugin says "Standby (Another instance active)".**
DAWStream is loaded in more than one place (another track, another project tab, another DAW). Only one can stream at a time — click **Take Over** on the one you want.

**The plugin doesn't show up in my DAW.**
Rescan plugins first. If it's still missing, your DAW probably only looks in `C:\Program Files\Common Files\VST3` — run `Install.bat` as administrator and rescan again.

**"Open App" in the plugin shows an error.**
- *"DAWStream.exe not found"* — open DAWStream once from the desktop shortcut. After that, the button will find it.
- *"DAW is elevated"* — your DAW runs as administrator. For safety, the plugin then only launches the app from `C:\Program Files`. Run `Install.bat` as administrator, or just open the app yourself.

**The video freezes.**
The DAW window is minimized. It can be behind other windows, but it can't be minimized.

**The stream audio is a little behind the video.**
The app keeps a small (~20 ms) buffer so the audio doesn't crackle, plus whatever delay Discord adds. Your own monitoring through ASIO is not affected.

## Building from source

You need [Rust](https://rustup.rs/) (stable) on Windows x64.

```powershell
# Plugin (VST3 + CLAP)
cargo xtask bundle dawstream-vst3 --release

# App
cargo build --release -p dawstream-app

# Tests
cargo test --workspace
```

Output:

- `target/bundled/dawstream-vst3.vst3`
- `target/bundled/dawstream-vst3.clap`
- `target/release/dawstream-app.exe`

`Install.bat` expects the files to be named `DAWStream.vst3`, `DAWStream.clap` and `DAWStream.exe`, so rename them (or copy them by hand) if you want to install a local build.

### Project layout

| Folder | What's in it |
|---|---|
| `crates/dawstream-vst3` | The plugin, built with [nih-plug](https://github.com/robbert-vdh/nih-plug) |
| `crates/dawstream-app` | The companion app (window mirror + WASAPI playback), built with egui |
| `crates/dawstream-ipc` | Shared-memory ring buffer the plugin and app use to talk to each other |
| `xtask` | Build helper for bundling the plugin |
| `assets` | Logo, wordmark and the app icon (`icon.ico` is embedded into the exe at build time) |

## License

[GPL-3.0](LICENSE)
