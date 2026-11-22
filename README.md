<h1 align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/wordmark-dark.svg">
    <img src="assets/wordmark-light.svg" alt="DAWStream" width="480">
  </picture>
</h1>

You share your DAW on Discord and your friends see it but hear nothing. That's because your DAW plays through ASIO and Discord can't hear ASIO.

DAWStream fixes that. Put the plugin on your master, open the app, share the app's window in Discord. Your friends get the screen and the sound. You keep monitoring through your interface like always.

No Voicemeeter, no VB-Cable, no OBS. Windows only.

![DAWStream streaming FL Studio in a Discord call](docs/images/discord.jpg)

## Setup

1. Download the zip from [Releases](../../releases), unzip it, run `Install.bat`.
2. Rescan plugins in your DAW and put **DAWStream** last on the master.
3. Click **Open App** in the plugin.
4. In the app, set **Audio Output** to something you're not listening on. Your monitor's speakers or `Realtek Digital Output` are good picks (the app marks them `[Recommended]`).
5. In Discord: **Share Screen → Applications → DAWStream**.

When it's working, both the plugin and the app say **LIVE**:

<img src="docs/images/plugin.jpg" alt="Plugin showing LIVE" width="380">

A few things worth knowing:

- Press **H** in the app to hide the top bar so people only see your DAW.
- **AUDIO** and the slider in the plugin only affect what the stream hears, not your own mix.
- Save the plugin in your default template and you never have to think about it again.

## Troubleshooting

**I hear everything twice.**
The app's Audio Output is set to the interface you're listening on (it shows `[!]` next to those). Pick another one:

<img src="docs/images/audio-output.jpg" alt="Audio Output list" width="500">

**I don't have a spare output.**
Install [VB-Cable](https://vb-audio.com/Cable/) and pick `CABLE Input`. That's it, no routing.

**The plugin doesn't show up.**
Your DAW probably only scans `C:\Program Files\Common Files\VST3`. Right-click `Install.bat` → *Run as administrator*, then rescan.

**The app says "Waiting for Plugin".**
The plugin isn't on the master, or your DAW's audio engine is off.

**The plugin says "Standby".**
You have DAWStream loaded in more than one place. Click **Take Over** on the one you want to stream.

**The stream video is frozen.**
Your DAW is minimized. It can be behind other windows, just not minimized.

**Pro Tools?**
Doesn't work. Pro Tools only loads AAX plugins.

## Technical details

<details>
<summary><b>How it works</b></summary>

<br>

The plugin (VST3/CLAP) sits on the master and passes your audio through untouched. On the side it writes a stereo copy into a lock-free ring buffer in Windows shared memory.

The app reads that buffer and plays it with WASAPI on whatever output you picked, with about 20 ms of buffering. At the same time it captures your DAW window with `PrintWindow` at up to 60 fps and draws it in its own window.

Discord captures audio per application, so when you share the DAWStream window it gets both the picture and the sound. Your ASIO monitoring never goes through any of this, so it doesn't get any extra latency.

Only one plugin instance writes at a time. The others go into standby, which is what the **Take Over** button switches.

</details>

<details>
<summary><b>Building from source</b></summary>

<br>

You need Windows x64 and [Rust](https://rustup.rs/) (stable, MSVC toolchain).

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

To install a local build with `Install.bat`, put it next to these files, renamed to `DAWStream.vst3`, `DAWStream.clap` and `DAWStream.exe` (it also looks in a `dist` folder next to itself). Close the app and your DAW before running it, or the files will be locked.

Releases are built by [GitHub Actions](.github/workflows/release.yml) when a `v*` tag is pushed. The zip comes with a `SHA256SUMS.txt`.

</details>

<details>
<summary><b>Project layout</b></summary>

<br>

| Folder | What's in it |
|---|---|
| `crates/dawstream-vst3` | The plugin, built with [nih-plug](https://github.com/robbert-vdh/nih-plug) |
| `crates/dawstream-app` | The app: window capture, WASAPI playback, egui UI |
| `crates/dawstream-ipc` | The shared-memory ring buffer both sides use |
| `xtask` | Plugin bundling helper |
| `assets` | Logo and app icon (`icon.ico` is embedded into the exe at build time) |
| `docs/images` | README screenshots |

</details>

<details>
<summary><b>Where Install.bat puts things</b></summary>

<br>

| | Normal | Run as administrator |
|---|---|---|
| App | `%LOCALAPPDATA%\DAWStream` | `C:\Program Files\DAWStream` |
| VST3 | `%LOCALAPPDATA%\Programs\Common\VST3` | `C:\Program Files\Common Files\VST3` |
| CLAP | `%LOCALAPPDATA%\Programs\Common\CLAP` | `C:\Program Files\Common Files\CLAP` |

If you've installed both ways at some point, your DAW may load the older copy. Delete the one you don't use.

If your DAW runs as administrator, the plugin's **Open App** button only launches the app from `C:\Program Files`, so use the admin install.

</details>

## License

[GPL-3.0](LICENSE)
