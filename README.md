# Soundboard TUI

Soundboard TUI is a terminal soundboard for Linux. It sends audio to a PipeWire virtual microphone, so people in a voice call can hear it, and can optionally play the same audio through your speakers or headphones.

The app includes a guided audio setup, a managed sound library, per-sound volume, aliases, and a compact quick-play mode.

## Platform support

Soundboard TUI currently supports **Linux systems running PipeWire**. It uses Unix sockets and PipeWire-specific tools, so macOS and Windows are not supported.

The app expects these commands to be available:

- `pw-dump`
- `pactl`, connected to `pipewire-pulse`
- `mpv`, built with PipeWire audio output support

You also need Rust 1.85 or newer to install from source.

On Ubuntu and Debian-based distributions, the required packages are usually available with:

```bash
sudo apt install pipewire pipewire-pulse pipewire-bin pulseaudio-utils mpv
```

Package names differ between distributions. After installation, confirm that `pw-dump`, `pactl info`, and `mpv --version` run successfully in your terminal.

## Install

Install the latest version directly from GitHub:

```bash
cargo install --locked --git https://github.com/BernardoSKropiwiec/tui_soundboard.git
```

Make sure Cargo's binary directory is on your `PATH`. It is usually `~/.cargo/bin`.

To build without installing:

```bash
git clone https://github.com/BernardoSKropiwiec/tui_soundboard.git
cd tui_soundboard
cargo run --release
```

## First run

Start the application with:

```bash
soundboard_tui
```

On the first run, the audio setup checks PipeWire, `pipewire-pulse`, and `mpv`. You can then use an existing virtual microphone or let Soundboard TUI create one.

When creating a managed virtual microphone, the app asks which physical microphone to mix with the soundboard. It writes `90-soundboard-tui.conf` under your PipeWire configuration directory and restarts the user `pipewire-pulse` service. Select `soundboard_tui_mix.monitor` as the microphone in Discord or another voice application.

Press `a` in configuration mode to import audio. Imported files are copied into the managed library, so the originals can be moved or deleted afterward. Supported extensions are AAC, FLAC, M4A, MP3, OGG, Opus, and WAV.

## Controls

### Configuration mode

| Key | Action |
| --- | --- |
| Arrow keys | Select a sound pad |
| `Enter` or `Space` | Play the selected sound |
| `s` | Stop playback |
| `+` / `-` | Change the selected sound's volume |
| `a` | Import audio files |
| `e` | Edit the name and aliases |
| `d` | Permanently delete the selected imported file |
| `m` | Toggle playback through the local output |
| `A` | Open audio setup |
| `r` | Reload the sound library |
| `Ctrl+k` | Open quick-play mode |
| `q` or `Esc` | Exit |

### Quick-play mode

Run the app directly in quick-play mode with:

```bash
soundboard_tui --quick
```

Type to filter by sound name, file name, or alias. Use `Up` and `Down` to select a result, then press `Enter` to play it. The TUI closes after playback starts, while the background `mpv` process remains available for the next command. Press `Ctrl+k` to return to configuration mode or `Esc` to close without playing.

Global shortcuts and floating-window rules are not installed by Soundboard TUI. If you want a shortcut such as `Super+b`, configure your compositor or desktop environment to launch `soundboard_tui --quick` in a terminal.

## Files and processes

Soundboard TUI follows the XDG base directories when their environment variables are set. The default locations are:

| Purpose | Default location |
| --- | --- |
| Imported audio and library metadata | `~/.local/share/soundboard-tui/` |
| Settings | `~/.local/share/soundboard-tui/settings.toml` |
| `mpv` logs | `~/.local/state/soundboard-tui/` |
| `mpv` sockets and playback lock | `$XDG_RUNTIME_DIR/` or the system temporary directory |
| Managed PipeWire configuration | `~/.config/pipewire/pipewire-pulse.conf.d/90-soundboard-tui.conf` |

Soundboard TUI keeps idle `mpv` processes running between plays. Audio imports copy files into the managed library, and deleting a sound from the app permanently removes that managed copy.

## Troubleshooting

### PipeWire is unavailable

Check that the user services are running:

```bash
systemctl --user status pipewire pipewire-pulse
pactl info
pw-dump
```

If `pactl info` does not report PipeWire as its server, enable or install your distribution's `pipewire-pulse` compatibility service.

### `mpv` cannot play through PipeWire

Confirm that your build includes the PipeWire audio output:

```bash
mpv --no-config --ao=help
```

The output must list `pipewire`. Playback errors are recorded in `mpv-mix.log` and `mpv-local.log` under the state directory shown above.

### The virtual microphone is missing

Open audio setup with `A` and press `r` to run diagnostics again. If you use the managed mix, remove it with `x`, then create it again and reselect `soundboard_tui_mix.monitor` in your voice application.

### Audio reaches the call but not your speakers

Local playback is optional and can fail independently of virtual microphone playback. Press `m` to enable the local monitor and check `mpv-local.log` if it still fails.

## Uninstall

Before removing the binary, open audio setup with `A` and press `x` to remove the managed virtual microphone. Then run:

```bash
cargo uninstall soundboard_tui
rm -rf "${XDG_DATA_HOME:-$HOME/.local/share}/soundboard-tui"
rm -rf "${XDG_STATE_HOME:-$HOME/.local/state}/soundboard-tui"
```

The data removal commands permanently delete imported sounds, metadata, settings, and logs.

## License

Licensed under the [MIT License](LICENSE).
