# dictate

Personal push-to-talk dictation for Windows. Hold a key, speak, release — the text appears wherever you were typing.

No window, no browser engine, no settings screen. Configuration is one TOML file; feedback is a tray icon and a small bubble that reacts to your voice. That is the whole reason it is around a thousand lines rather than tens of thousands.

Built for myself. Published because it might be a useful starting point for someone else.

## What it does

```
hold hotkey  →  record mic  →  release  →  transcribe locally  →  type into the focused app
```

```mermaid
flowchart LR
    K["hold hotkey"] --> R["record mic"]
    R --> U["release"]
    U --> G{"long enough,<br/>and not silence?"}
    G -- no --> X["ignore"]
    G -- yes --> T["transcribe locally"]
    T --> I["type into<br/>the focused app"]
    I --> C["restore your<br/>previous clipboard"]
```

- **Fully local.** Speech recognition runs on your machine. Nothing is uploaded.
- **Reacts to your voice.** The bubble's bars follow your actual microphone level, so silence is flat and speech moves them.
- **Stays out of the way.** The bubble never takes focus, so the caret stays in the app you are dictating into.
- **Correct clipboard handling.** Text delivery goes through [`win-text-inject`](https://crates.io/crates/win-text-inject), which restores your previous clipboard only after the target has read the new text, and keeps transcripts out of Windows clipboard history.

## Architecture

Five files, each one job.

```
main.rs       the loop, and the resident model
audio.rs      microphone capture, downmix, level metering, resample to 16 kHz
config.rs     dictate.toml
autostart.rs  the HKCU Run key
ui.rs         tray icon and the voice-reactive bubble
```

Three threads, so the parts that must not block each other never do: the keyboard hook, the audio callback, and the UI message pump each run on their own, and the main loop coordinates them.

```mermaid
flowchart TD
    subgraph hook["handy-keys thread"]
        HK["keyboard hook<br/>press / release events"]
    end
    subgraph main["main thread"]
        L["event loop (main.rs)"]
        E["Engine: model held<br/>resident (transcribe-cpp)"]
    end
    subgraph audio["audio thread (audio.rs)"]
        CAP["cpal capture callback<br/>downmix + level meter"]
    end
    subgraph uithread["UI thread (ui.rs)"]
        WIN["tray icon + bubble<br/>Win32 message pump"]
    end

    HK -- "press / release" --> L
    L -- "start / finish" --> CAP
    CAP -- "live input level" --> WIN
    CAP -- "16 kHz samples" --> L
    L -- "recording / transcribing / hide" --> WIN
    L --> E
    E -- "text" --> L
    L -- "inject" --> INJ["win-text-inject<br/>→ focused app"]
```

Almost everything is thin glue over a crate. `ui.rs` is the largest file because the tray icon and the bubble are raw Win32 GDI, which nothing does for you, and because the bubble has one hard requirement — it must never steal focus, or the text lands nowhere.

| Job | Crate | Notes |
|---|---|---|
| Global hold-to-talk hotkey (incl. modifier-only bindings) | [`handy-keys`](https://crates.io/crates/handy-keys) | reports press *and* release, which `RegisterHotKey` cannot |
| Microphone capture | [`cpal`](https://crates.io/crates/cpal) | its callback also feeds the bubble's level meter |
| Resampling to 16 kHz | [`rubato`](https://crates.io/crates/rubato) | anti-aliased, not naive decimation |
| Speech recognition | [`transcribe-cpp`](https://crates.io/crates/transcribe-cpp) | ggml; loads the same GGUF models Handy uses |
| Text delivery | [`win-text-inject`](https://crates.io/crates/win-text-inject) | built for exactly this; see below |

### Why the text-delivery step is its own crate

Pasting a transcript into whatever app has focus is the step every dictation tool gets subtly wrong, so it lives in a separate, tested crate. Everything it fixes is something this app hits *by construction*:

```mermaid
flowchart TD
    A["transcript ready"] --> B{"focused window<br/>elevated?"}
    B -- yes --> B1["leave on clipboard,<br/>prompt Ctrl+V<br/>(UIPI would eat it silently)"]
    B -- no --> C["release the still-held<br/>hotkey modifier<br/>(or Ctrl+V becomes AltGr+V)"]
    C --> D["publish text as a<br/>delayed-render promise<br/>+ privacy formats"]
    D --> E["synthesize paste"]
    E --> F["target reads the clipboard"]
    F --> G["restore your previous<br/>clipboard, after the read"]
```

- The hotkey modifier is **held by construction** when this app pastes — it is a push-to-talk key you are still pressing. Sanitizing it is not an edge case here, it is every single press.
- Because delivery is a paste, your clipboard would be **destroyed on every dictation** without the delayed-render restore. That is [Handy issue #502](https://github.com/cjpais/Handy/issues/502), and the fix is the whole reason this app does not have it.
- Every sentence you speak passes through the clipboard, so without the four privacy opt-out formats it would land in **Windows clipboard history and cloud sync** — quietly breaking the "fully local" promise.

## Install and use

You need two things in one folder: `dictate.exe` and a speech model. That is the whole install. The default build links the recognition engine into the executable, so there are no DLLs to ship and no runtime to install.

### 1. Get `dictate.exe`

Either grab a release binary, or build it (see [Building](#building) below). Put it in a folder you will keep, for example `C:\Users\you\Apps\dictate\`. Do not run it from a Downloads or temp folder if you plan to enable autostart, since those get cleaned.

### 2. Get a model

`dictate` uses GGUF speech models — the same ones [Handy](https://github.com/cjpais/Handy) uses. Download one `.gguf` file from [huggingface.co/handy-computer](https://huggingface.co/handy-computer) and put it next to the exe. Good starting points:

| Model | Size | Notes |
|---|---|---|
| [`canary-180m-flash`](https://huggingface.co/handy-computer/canary-180m-flash-gguf) | ~200 MB | tiny and instant; English, German, Spanish, French |
| [`parakeet-tdt-0.6b-v3`](https://huggingface.co/handy-computer/parakeet-tdt-0.6b-v3-gguf) | ~740 MB | more accurate; 25 European languages |

Download the `Q8_0` file (best size/quality balance). Nothing downloads automatically — the model is yours, placed by you, and never fetched behind your back.

### 3. First run

Run `dictate.exe` once. It writes a commented `dictate.toml` next to itself and stops. Open that file, set `model` to your `.gguf` path, and run again:

```toml
hotkey = "CtrlLeft+WinLeft"
model  = "C:/Users/you/Apps/dictate/canary-180m-flash-Q8_0.gguf"
language = "en"
```

Forward slashes or escaped backslashes both work in the path.

### 4. Dictate

Hold the hotkey, speak, release. A small bubble appears bottom-center and its bars follow your voice. The text lands wherever your cursor was. Right-click the tray icon to open the config or exit.

### Start on login

Set `autostart = true` in `dictate.toml` and restart it once. It writes an entry under `HKCU\...\Run` pointing at wherever the exe currently lives, and re-asserts it on every launch. Set it back to `false` and restart to remove the entry. This is why the exe should live in a permanent folder before you enable it.

## Building

Needs the Rust MSVC toolchain, plus CMake and the MSVC C++ build tools, because `transcribe-cpp` compiles a ggml engine from source.

```
cargo build --release
```

The result is a self-contained CPU build: `target/release/dictate.exe`, no DLLs. Canary 180M runs comfortably in real time on a modern CPU, so the GPU is not needed.

**Optional GPU acceleration.** For a larger model on a machine with a GPU, build with the Vulkan backend:

```
cargo build --release --features vulkan-gpu
```

That produces loadable ggml backend DLLs; they must sit next to the exe, and building them also needs the Vulkan SDK installed. For most people the CPU build is simpler and fast enough.

## Configuration

On first run `dictate` writes a commented `dictate.toml` next to the executable and stops so you can fill it in.

```toml
# Hold this to record. Modifier-only bindings work and are usually the most
# comfortable, because they cannot collide with an application shortcut.
#   "CtrlLeft+WinLeft"   "CtrlRight"   "AltRight"   "Ctrl+Space"   "F13"
hotkey = "CtrlLeft+WinLeft"

# Absolute path to a GGUF speech model. Any model transcribe-cpp supports works;
# Parakeet and Canary are good CPU choices. Handy's models live under
# ~/.cache/huggingface/hub if you already have it installed.
model = "C:/path/to/model.gguf"

# Language hint. Comment out to let the model detect, which is less accurate on
# short utterances.
language = "en"

# Ignore recordings shorter than this, so an accidental tap does not produce a
# hallucinated transcript.
min_recording_ms = 400

# Keep the model resident between dictations. Costs idle RAM, saves seconds per
# dictation, since loading dominates everything else.
keep_model_loaded = true

# Start with Windows. Toggling this and restarting is enough in either direction.
autostart = false
```

The tray icon's right-click menu opens this file and exits the app.

## Deliberately not here

This is a personal tool, and the absences are the point.

- No model manager or download UI — put a `.gguf` path in the config.
- No settings window — it is a text file.
- No cross-platform support — Windows only.
- No auto-update, no telemetry, no account.

## Status

Works, and used daily by its author. Rough edges remain: the bubble's shape and animation are tuned by hand-editing constants, there is no voice-activity trimming, and the paste-chord table is small. Contributions and forks welcome, but it is shaped for one person's use first.

## License

MIT OR Apache-2.0.
