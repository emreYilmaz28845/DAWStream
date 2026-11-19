# DAWStream

Stream **any Windows DAW** (FL Studio, Ableton Live, Cubase, Nuendo, REAPER, Studio One, Bitwig Studio, Pro Tools, Reason, Cakewalk, etc.) to **Discord** with **high-quality stereo ASIO audio** and **zero extra latency** — without Voicemeeter, VB-Cable, OBS, or double monitoring.

## Why DAWStream?

Windows DAWs use **ASIO** for low-latency audio, which bypasses the Windows audio mixer (WASAPI). Discord's screen share only captures WASAPI audio from the shared application window — resulting in silent DAW streams.

**DAWStream** solves this cleanly in two lightweight parts:

1. **DAWStream Plugin (`VST3` & `CLAP`)** — Placed at the end of your DAW's Master channel. It passes your ASIO audio through untouched (zero latency) while copying a stereo feed into a lock-free Windows shared-memory ring buffer.
2. **DAWStream Companion App (`DAWStream.exe`)** — Mirrors your DAW window at up to 60 FPS and plays the audio stream via Windows WASAPI to an unused/silent output (such as `Realtek Digital Output` or an unused HDMI/monitor output).

When you share the **DAWStream** window in Discord, your friends receive clean video + full stereo audio while you keep monitoring directly through your ASIO interface with zero echo.

---

## Quick Start

1. Download the latest `DAWStream-*-windows-x64.zip` from [**Releases**](../../releases) and extract it anywhere (e.g. `C:\Program Files\DAWStream` or your preferred folder).
2. Run **`Install.bat`** *(right-click → **Run as administrator** if your DAW only scans `C:\Program Files\Common Files\VST3`)*.
3. Open your DAW, rescan plugins, and add **DAWStream** (`VST3` or `CLAP`) to the end of your **Master** channel.
   - *Tip: Save it in your DAW's default template so you never have to add it again. When `DAWStream.exe` is closed, the plugin uses negligible CPU and does not affect your audio.*
4. Launch **DAWStream** from your Desktop shortcut (or click **Open App** inside the plugin).
5. In `DAWStream.exe`:
   - Select your **DAW window** (auto-detected).
   - Select a **`[Recommended]`** silent audio output (e.g. `Realtek Digital Output` or an unused monitor output — avoid selecting your main ASIO interface to prevent hearing double audio).
6. In **Discord**: **Share Screen → Applications → `DAWStream — Discord DAW Streaming`**.
7. Press **`H`** inside DAWStream to hide the top toolbar for a clean fullscreen view (move your mouse to the top edge or press **`H`** again to restore).

---

## Supported DAWs

Works with any 64-bit Windows DAW supporting **VST3** or **CLAP**, including:
- FL Studio
- Ableton Live
- Steinberg Cubase / Nuendo
- REAPER
- PreSonus Studio One
- Bitwig Studio
- Reason
- Cakewalk / BandLab
- Tracktion Waveform, Mixcraft, Renoise, Ardour, LMMS, and more.

---

## FAQ / Troubleshooting

**Q: I hear double audio (echo) in my headphones!**  
Make sure **Audio Output** in `DAWStream.exe` is NOT set to your main ASIO interface (Focusrite, Topping, Audient, etc.). Select an unused output such as `Realtek Digital Output (S/PDIF)` or an unused monitor HDMI/DisplayPort audio device.

**Q: What if my PC has no unused audio outputs at all?**  
You can install any free virtual sink (such as VB-Cable) and select it as the Audio Output in DAWStream — you won't need any routing or Voicemeeter setup.

**Q: Windows Defender SmartScreen showed a warning on `DAWStream.exe`.**  
The release binaries are unsigned open-source builds compiled directly by GitHub Actions from this repository. You can verify the workflow in `.github/workflows/release.yml` or build from source yourself.

**Q: My DAW isn't showing the plugin after running `Install.bat`.**  
Right-click `Install.bat` and choose **Run as administrator** so it installs to both `%LOCALAPPDATA%\Programs\Common\VST3` and `C:\Program Files\Common Files\VST3`, then rescan plugins in your DAW.

---

## Building from Source

Requires [Rust](https://rustup.rs/) (stable) on Windows x64:

```powershell
# Build VST3 & CLAP plugins
cargo xtask bundle dawstream-vst3 --release

# Build companion app
cargo build --release -p dawstream-app
```

Outputs are generated in:
- `target/bundled/dawstream-vst3.vst3`
- `target/bundled/dawstream-vst3.clap`
- `target/release/dawstream-app.exe`

---

## License

Licensed under the **GNU General Public License v3.0 (GPL-3.0)** — see [LICENSE](LICENSE) for details.
