# AGENTS.md

Source-of-truth instructions for coding agents in this repository.

## Project Scope

Dikt is a **Fedora Workstation + GNOME + Wayland** speech-to-text project with IBus integration.

Targets in scope:
- `dikt` (GTK4/libadwaita preferences UI)
- `dikt --daemon` (recording/transcription D-Bus runtime)
- `ibus-dikt-engine` (IBus engine process)

Out of scope unless explicitly requested:
- Non-GNOME desktop support
- Non-Wayland input stack support
- Generic cross-distro abstractions that weaken Fedora/GNOME behavior

## Build, Test, Run

### Fedora dependencies

```bash
sudo dnf install -y \
    gtk4-devel \
    libadwaita-devel \
    graphene-devel \
    alsa-lib-devel \
    pipewire-devel \
    libevdev-devel \
    openssl-devel \
    ibus-devel \
    cmake \
    clang-devel \
    glslc
```

### Developer commands

```bash
# Build
cargo build
cargo build --release

# Quality gates
cargo fmt --all
cargo clippy --all-targets --all-features -- -D warnings
cargo test

# Run UI
cargo run --release

# Run daemon
cargo run --release -- --daemon

# Run IBus engine (dev/testing)
cargo run --release --bin ibus-dikt-engine --features cli -- --ibus
```

### Model bootstrap for development

```bash
mkdir -p resources/models
curl -o resources/models/silero_vad_v4.onnx https://github.com/rohithmahesh3/Dikt/releases/download/models/silero_vad_v4.onnx
```

## Packaging Workflow

RPM build entrypoint:

```bash
./build-rpm.sh
```

`build-rpm.sh` generates these files from templates at build time:
- `packaging/fedora/ibus-dikt.spec` from `packaging/fedora/ibus-dikt.spec.in`
- `packaging/fedora/dikt.xml` from `packaging/fedora/dikt.xml.in`

Generated files are not intended to be committed.

## Runtime Architecture

### Process model

- `dikt`: preferences UI only.
- `dikt --daemon`: owns recording state, transcription, D-Bus API, global toggle shortcut runtime, evdev keyboard monitoring.
- `ibus-dikt-engine`: IBus callbacks and commit path to focused app.

### D-Bus contract

Service:
- Bus: `io.dikt.Transcription`
- Path: `/io/dikt/Transcription`
- Interface: `io.dikt.Transcription`

Methods:
- `StartRecordingSessionForTarget(u64 target_engine_id) -> (u64 session_id, string claim_token)`
- `StopRecordingSession(u64 session_id) -> bool`
- `CancelRecordingSession(u64 session_id) -> bool`
- `GetState() -> (bool is_recording, bool has_model_selected)`
- `GetToggleDiagnostics() -> (bool, string, string, string, u64, bool, bool, u64, u64, u64)`
- `GetToggleDiagnosticsVerbose() -> string` (JSON)
- `GetToggleRecentEvents() -> array<string>`
- `GetSessionStatus(u64 session_id) -> (string state, string message, u64 updated_ms)`
- `TakePendingCommitForSession(u64 session_id, string claim_token) -> (bool has_text, string text)`
- `GetPendingCommitStats() -> string` (JSON)
- `GetLivePreeditForSession(u64 session_id, string claim_token) -> (u64 revision, bool visible, string text)`
- `GetActiveSessionForEngine(u64 engine_id) -> (u64 session_id, string claim_token, bool allow_preedit)`
- `SetFocusedEngine(u64 engine_id, bool focused)`
- `GetFocusedEngine() -> (u64 focused_engine_id, u64 last_change_ms)`
- `GetRecentLogs() -> array<string>`
- `GetLanguage() -> string`
- `SetLanguage(string)`

Signals:
- `TranscriptionReady(string)`
- `RecordingStateChanged(bool)`
- `Error(string)`

### Pending commit handoff

`DiktState` stores final transcripts in a bounded `pending_commit` queue consumed via
`TakePendingCommitForSession`.

Important behavior:
- Start recording does **not** clear pending commit.
- `pending_commit` stores `(session_id, claim_token, text)` and keeps up to 32 items, dropping oldest when full.
- Queue consume is session-claim scoped; a consumer must present both session id and claim token.
- Session metadata is retained for a bounded TTL and cleaned up for terminal states.
- Debug transcription testing does **not** drain pending commits.
- Toggle recording does **not** block on pending queue drain before starting a new session.

### Shortcut behavior

Dikt uses a global press-to-toggle dictation shortcut.

Settings keys used for the global shortcut:
- `dictation-shortcut-keyval` (GDK keyval stored in GSettings)
- `dictation-shortcut-modifiers` (GDK modifier bitmask)

Global toggle flow uses **evdev** (`src/global_shortcuts.rs`):
1. Discover keyboard devices in `/dev/input/event*`, open event streams.
2. Resolve GDK keyval+modifiers to evdev keycodes (`src/key_mapping.rs`).
3. On press while idle:
   - switch to Dikt engine (verified),
   - verify focused-context activation via daemon `GetFocusedEngine`,
   - call `StartRecordingSessionForTarget(focused_engine_id)` and store `(session_id, claim_token)`.
4. On next press while recording:
   - call `StopRecordingSession(session_id)` and wait for ack,
   - do **not** auto-restore input source in toggle flow.
5. Final text delivery:
   - engine-side listener resolves `(session_id, claim_token)` via `GetActiveSessionForEngine(engine_id)`,
   - live preedit polls `GetLivePreeditForSession(session_id, claim_token)`,
   - final commits poll `TakePendingCommitForSession(session_id, claim_token)`,
   - commits via `ibus_engine_commit_text` while engine is active.
6. `disable()` performs one final `TakePendingCommitForSession` using the last known session claim.

This architecture intentionally avoids autoswitch restore races.

This approach requires read access to `/dev/input/event*` devices. A udev rule
(`packaging/fedora/90-dikt-input.rules`) ensures `uaccess` for the active desktop user.

## Critical Constraints

1. Do not reintroduce shell-based input-source switching.
   Use FFI-backed helpers (`src/ibus_control.rs`, `ibus-sys/wrapper.c`).

2. Do not block IBus callback threads with long operations.
   Stop/transcribe work stays in daemon; engine-side workers enqueue UI/IBus commands that are applied on main-thread callbacks.

3. Preserve GObject lifetime safety in async commit paths.
   Keep the command-queue + timer callback pattern intact in `src/ibus_engine/context.rs`.

4. Keep evdev device lifecycle clean.
   Close device streams on session restart; abort reader tasks on config change.

5. Keep daemon state transitions consistent.
   `RecordingStateChanged(false)` should happen immediately when stop starts, not after long transcription.

6. Keep commit delivery single-path in toggle flow.
   Do not reintroduce direct restore/commit in `global_shortcuts.rs`.

## Settings and Feature Notes

Schema file: `data/io.dikt.Transcription.gschema.xml`.

Current active behavior:
- Toggle dictation recording
- Optional audio feedback sounds
- Optional LLM post-processing on final transcript

Removed/obsolete paths should not be reintroduced without product decision:
- `recording-mode` auto mode

## Key Source Files

Core:
- `src/main.rs`
- `src/app.rs`
- `src/dbus/server.rs`
- `src/settings.rs`

IBus and toggle path:
- `src/global_shortcuts.rs`
- `src/key_mapping.rs`
- `src/ibus_engine/context.rs`
- `src/ibus_control.rs`
- `src/bin/ibus-dikt-engine.rs`
- `ibus-sys/wrapper.c`
- `ibus-sys/wrapper.h`

Models/audio:
- `src/managers/model.rs`
- `src/managers/audio.rs`
- `src/managers/transcription.rs`
- `src/audio_toolkit/audio/recorder.rs`
- `src/audio_feedback.rs`

UI:
- `src/ui/window.rs`
- `src/ui/pages/general.rs`
- `src/ui/pages/models.rs`
- `src/ui/pages/advanced.rs`
- `src/ui/pages/debug.rs`

Packaging:
- `build-rpm.sh`
- `packaging/fedora/ibus-dikt.spec.in`
- `packaging/fedora/dikt.xml.in`
- `packaging/fedora/dikt.service`
- `packaging/fedora/io.dikt.Transcription.service`
- `packaging/fedora/90-dikt-input.rules`

## Definition of Done for Agent Changes

Before finalizing any non-trivial change:
1. Run `cargo fmt --all`.
2. Run `cargo clippy --all-targets --all-features -- -D warnings`.
3. Run `cargo test`.
4. If packaging/schema changed, ensure generated artifacts are handled correctly and docs remain aligned.
