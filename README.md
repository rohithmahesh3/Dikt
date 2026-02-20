# Dikt

**Speech-to-Text for GNOME/Wayland**

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)

Dikt is a native speech-to-text application for GNOME on Wayland. It integrates directly with IBus, letting you dictate into any application with a global keyboard shortcut.

![Dikt Screenshot](https://dikt.tequerist.com/imgs/GeneralPage.png)

## Features

- **Native IBus Integration** — Seamless input method switching during dictation
- **Global Dictation Shortcut** — Toggle recording from anywhere, automatic input switching
- **Offline Processing** — All speech recognition runs locally on your device
- **Multi-language Support** — 50+ languages supported
- **Multiple Recognition Engines** — Whisper, Parakeet, Moonshine, SenseVoice
- **GNOME-Native UI** — Built with GTK4 and Libadwaita
- **AI Post-Processing** — Optional LLM-based cleanup of transcripts

## Installation

### Fedora / RHEL / CentOS

```bash
# Add the repository
sudo dnf config-manager addrepo --from-repofile=https://rohithmahesh3.github.io/dikt-rpm/dikt.repo

# Install
sudo dnf install ibus-dikt
```

### From Source

```bash
# Dependencies (Fedora)
sudo dnf install -y \
    rustc cargo \
    gtk4-devel libadwaita-devel graphene-devel \
    alsa-lib-devel ibus-devel glib2-devel \
    openssl-devel cmake clang-devel glslc

# Build
git clone https://github.com/rohithmahesh3/Dikt.git
cd Dikt
cargo build --release
```

## Setup

1. Install Dikt (see Installation above)
2. Open Dikt from your application menu
3. Configure your dictation shortcut
4. Download a recognition model

That's it. Dikt automatically handles input method switching during transcription.

## Usage

1. Press your dictation shortcut to start recording
2. Speak naturally
3. Press the shortcut again to transcribe and insert text

Dikt automatically switches to its input method during transcription and switches back when done. The text appears in whichever application has focus.

## Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                         Dikt                                │
├─────────────────────────────────────────────────────────────┤
│                                                             │
│  ┌─────────────┐    ┌─────────────┐    ┌─────────────┐     │
│  │   evdev     │───▶│    D-Bus    │───▶│    IBus     │     │
│  │  shortcut   │    │   daemon    │    │   engine    │     │
│  └─────────────┘    └──────┬──────┘    └──────┬──────┘     │
│                            │                   │            │
│                     ┌──────▼──────┐            │            │
│                     │    Audio    │            │            │
│                     │   capture   │            │            │
│                     └──────┬──────┘            │            │
│                            │                   │            │
│                     ┌──────▼──────┐            │            │
│                     │ Transcribe  │            │            │
│                     │   Engine    │────────────┘            │
│                     └─────────────┘                         │
│                                                             │
└─────────────────────────────────────────────────────────────┘
                              │
                              ▼
                    ┌─────────────────┐
                    │  Any GTK/Qt/    │
                    │  application    │
                    └─────────────────┘
```

**Components:**

| Component | Role |
|-----------|------|
| `dikt --daemon` | D-Bus service, handles shortcuts and audio |
| `ibus-dikt-engine` | IBus input method, commits text to apps |
| `dikt` (GUI) | Preferences window |

## Recognition Models

Dikt supports multiple speech recognition backends:

| Model | Strengths | Languages |
|-------|-----------|-----------|
| **Whisper** (Small/Medium/Turbo) | High accuracy | 50+ |
| **Parakeet V3** | CPU-optimized, auto-detect language | 50+ |
| **Moonshine** | Fast, low-resource | English |
| **SenseVoice** | Optimized for CJK | Chinese, Japanese, Korean, English |

Models are downloaded on-demand from the preferences window.

## Configuration

Open Dikt from your application menu to configure:

- **Language** — Primary recognition language
- **Dictation Shortcut** — Global keybinding to toggle recording
- **Audio Feedback** — Sounds for start/stop events
- **Model Selection** — Choose and download recognition models
- **Post-Processing** — Optional AI cleanup via LLM

## Requirements

- GNOME on Wayland
- IBus (default on most GNOME installations)
- PulseAudio or PipeWire audio system
- Microphone

## Troubleshooting

<details>
<summary>Dictation shortcut not working</summary>

```bash
# Check daemon status
systemctl --user status dikt.service

# Restart if needed
systemctl --user restart dikt.service
```

Also ensure no other application is capturing your shortcut key.
</details>

<details>
<summary>No microphone access</summary>

```bash
# Add user to audio group
sudo usermod -aG audio $USER

# Log out and back in
```
</details>

<details>
<summary>Manual model installation</summary>

Place models in `~/.local/share/dikt/models/`:

- **Whisper**: `.bin` files directly
- **Parakeet/SenseVoice**: extract `.tar.gz` to subdirectory
</details>

## Development

```bash
# Build
cargo build

# Run daemon
cargo run -- --daemon

# Run GUI
cargo run

# Run IBus engine
cargo run --bin ibus-dikt-engine -- --ibus
```

## Roadmap

- [ ] Additional distribution packages (Arch, Debian, openSUSE)
- [ ] Wayland-only global shortcuts via portal
- [ ] Custom vocabulary support
- [ ] Real-time transcription preview

## Contributing

Contributions are welcome! Please feel free to submit issues or pull requests.

## License

MIT License — see [LICENSE](LICENSE) for details.

## Acknowledgments

- [Whisper](https://github.com/openai/whisper) by OpenAI
- [IBus](https://github.com/ibus/ibus) — Intelligent Input Bus
- [Handy](https://github.com/cjpais/handy) — Original inspiration for this project

---

<p align="center">
  <a href="https://dikt.tequerist.com">Website</a> •
  <a href="https://github.com/rohithmahesh3/Dikt/issues">Issues</a> •
  <a href="https://github.com/rohithmahesh3/Dikt/discussions">Discussions</a>
</p>
