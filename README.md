# stt-linux

[![CI](https://github.com/denysvitali/stt-linux/actions/workflows/ci.yml/badge.svg)](https://github.com/denysvitali/stt-linux/actions/workflows/ci.yml)

Local speech-to-text dictation for Wayland. Press a key, speak, and the text
appears in whatever you were typing into.

Everything runs on your machine. No network calls, no cloud inference, no
telemetry — the only thing that ever leaves your computer is the one-time model
download.

> **Status: in development.** Capture, transcription, the daemon,
> clipboard/`wtype` injection, silence-based auto-stop and the recording
> overlay with live transcript preview all work end to end. Tagged releases
> are produced by `release.yml`; every push to `master` also drops a fresh
> `stt` + `sttd` pair as a 30-day workflow artifact — see
> [`.github/RELEASE.md`](.github/RELEASE.md).

## Why

`VoiceInk` and `superwhisper` are macOS-only and proprietary. The Linux options
are either X11-era or, in the case of Tauri-based ports, have Wayland support
that is documented as "limited" — one of them has a recording overlay that
steals focus and thereby breaks its own paste step.

This is a pure-Rust daemon built for Wayland specifically, with the awkward
parts of the platform treated as the design problem rather than an afterthought.

## What it looks like

While you speak, a small recording overlay floats above everything — never
focusable, so it can never become the target of its own paste step — with a
live level meter until words arrive, then the transcript as it is being
recognised:

| Listening — the waveform reacts to your voice | A live transcript as you speak |
|:---:|:---:|
| ![Listening — a live waveform in the overlay](assets/overlay-meter.png) | ![A live transcript appearing in the overlay](assets/overlay-live.png) |

Both frames come straight from CI: whenever the overlay code changes, a
headless compositor runs the demo (`cargo run -p stt-overlay --example demo`)
and commits what `grim` captures, so the pictures here cannot go stale.

## Requirements

- A Wayland compositor
- PipeWire (or ALSA)
- `wl-clipboard` — for clipboard injection and the safety net
- `wtype` — for direct typing on wlroots compositors (Hyprland, sway, niri) and KDE

```sh
# Arch
sudo pacman -S wl-clipboard wtype
```

## Install

There are two binaries: `stt`, the CLI your keybind calls, and `sttd`, the
daemon that holds the model. You need both.

With `cargo install` — no clone, and it puts them in `~/.cargo/bin`:

```sh
cargo install --git https://github.com/denysvitali/stt-linux stt sttd
```

From a [release](https://github.com/denysvitali/stt-linux/releases) —
grab the latest `stt-*` and `sttd-*` `.tar.xz` plus its `.sha256`, then:

```sh
tar -xJf stt-vX.Y.Z-x86_64-unknown-linux-gnu.tar.xz -C ~/.local/bin
tar -xJf sttd-vX.Y.Z-x86_64-unknown-linux-gnu.tar.xz -C ~/.local/bin
```

Or track `master` straight from the Actions tab: the
[`binaries` workflow](.github/workflows/binaries.yml) uploads a fresh
`stt` and `sttd` pair on every push (30-day retention).

From a clone, if you already have one:

```sh
cargo install --path crates/stt
cargo install --path crates/sttd
```

Or with the Makefile, which installs into `~/.local/bin` instead and takes the
usual `PREFIX`/`DESTDIR` variables, so it is also the packaging entry point:

```sh
make install                       # ~/.local/bin
sudo make install PREFIX=/usr/local
make uninstall
```

The default build runs inference on the CPU, which is what the timings below
assume. To use an ONNX Runtime execution provider instead, build with the
matching feature — `openvino` for Intel iGPUs, `webgpu` for a portable GPU
path, or `cuda`:

```sh
cargo install --path crates/sttd --features openvino
make install FEATURES=openvino
```

Then set `execution_provider` in the config to match.

Whichever route you took, fetch the model (~610 MB, int8 weights):

```sh
stt model download
```

It lands in `~/.local/share/stt-linux/models` and is not touched by
uninstalling; delete that directory to reclaim the space.

## Check your setup

```sh
stt doctor
```

This is the first thing to run when anything misbehaves. It reports your
compositor, which Wayland protocols it advertises, which audio device will
actually be recorded from, whether the model is present, and which injection
and activation backends are available — each with the reason it is or is not
usable. It exits non-zero if the session cannot dictate yet.

## Run it

Start the daemon (it keeps the model resident, which is the whole point):

```sh
sttd
```

Then bind a key in your compositor. On Hyprland:

```ini
# Toggle: press to start, press again to stop
bind  = SUPER, D, exec, stt toggle

# Or push-to-talk: hold to record, release to transcribe
bind  = SUPER, D, exec, stt start
bindr = SUPER, D, exec, stt stop

# Panic button
bind  = SUPER SHIFT, D, exec, stt cancel
```

`stt start` and `stt stop` both return in well under a millisecond —
transcription happens on a background thread — so the keybind never blocks.

## Configuration

`~/.config/stt-linux/config.toml`. Every field has a default, so the file is
optional. Write a fully-populated one with:

```sh
stt config --init
```

The settings most worth knowing about:

```toml
[audio]
# "default" prefers the PipeWire PCM, which is usually what you want.
# `stt doctor` shows what it resolves to; override with a device id.
device = "default"

[inject]
# Tried in order; the first one this session supports wins.
#   wtype       — types directly (wlroots, KDE)
#   clipboard   — copies, then sends the paste chord
#   copy_only   — copies and stops there; never types
backends = ["wtype", "clipboard"]

# Terminals usually need "ctrl+shift+v".
paste_keys = "ctrl+v"

# Always leave the transcript on the clipboard, whichever backend runs, so a
# misdirected or failed injection never loses your words.
always_copy = true

[overlay]
# The recording indicator. Set enabled = false if you want dictation with no
# visual feedback at all.
anchor = "bottom"           # "bottom", "top" or "center"
```

### Clipboard-only mode

If you would rather paste deliberately than have text appear:

```toml
[inject]
backends = ["copy_only"]
```

## How it works

```
        compositor keybind
                │
                ▼
        stt (CLI) ──unix socket──► sttd (daemon, model resident)
                                     │
                                     ├─ cpal ─► resample 16 kHz ─► Parakeet
                                     │
                                     └─► injection backend ─► your window
```

The daemon exists because the ASR model takes about 1.7 s to load. Loading it
once and keeping it in memory is what makes dictation feel instant — inference
itself runs at roughly 19x realtime on CPU, so a 10-second utterance becomes
text in about half a second.

The overlay is a `wlr-layer-shell` surface with `KeyboardInteractivity::None`:
it sits above fullscreen windows but can never take focus, so it can never
steal the very keystrokes it exists to accompany. It is rendered by hand onto
a shared-memory buffer — no GUI toolkit — which is why it runs as a thread
inside the daemon instead of a separate process.

### Two things it is careful about

**It will not type into the wrong window.** Injection sends keystrokes to
whatever is focused *at the moment it runs*, which is not necessarily the window
you dictated into. The daemon records which window was focused when you started
speaking, checks again before injecting, and falls back to clipboard-only if
focus moved. Worst case you paste manually; you never find your words in a
different application.

**It will not lose a transcript.** Every transcript goes to the clipboard
regardless of which backend runs, so a failed injection is recoverable.

## The model

[Parakeet TDT 0.6B v3](https://huggingface.co/nvidia/parakeet-tdt-0.6b-v3) by
NVIDIA, in [ONNX form](https://huggingface.co/istupakov/parakeet-tdt-0.6b-v3-onnx),
int8-quantized. 25 European languages with automatic detection.

Chosen over Whisper for two reasons: it is several times faster on CPU, which
matters when there is no CUDA; and being a transducer it emits nothing during
silence, where Whisper's decoder is prone to hallucinating text. The model is
not bundled — it is NVIDIA's, under its own licence — so the first run downloads it.

## Development

```sh
cargo test                                    # unit and integration tests
cargo test -p stt-core --features model-tests # golden-audio tests (needs the model)
cargo clippy --all-targets
```

The golden-audio tests assert on word error rate rather than exact strings: ASR
output is not bit-stable across `ort` and model revisions, so an exact match
would fail for the wrong reasons.

Useful for debugging the pipeline in isolation:

```sh
stt record --out /tmp/t.wav --seconds 5   # capture only
stt transcribe /tmp/t.wav --bench         # transcription only, with timings
```

If dictation produces nothing, run these two in order — they tell you whether
the problem is the microphone or the model.

To iterate on the overlay in isolation, without a daemon or a microphone:

```sh
cargo run -p stt-overlay --example demo   # sweeps the meter, then a transcript
```

## Licence

MIT OR Apache-2.0. The ASR models are NVIDIA's and carry their own terms.
