//! Configuration, read from `dictate.toml` next to the executable.
//!
//! A file rather than a settings UI: this is a personal tool, and a UI is the single largest
//! source of complexity in every dictation app that has one.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Hold-to-talk binding, e.g. `"Ctrl+Space"`, `"CtrlRight"`, `"F13"`.
    ///
    /// A modifier-only binding like `"CtrlRight"` is allowed and is usually the most comfortable,
    /// since it cannot collide with an application shortcut.
    pub hotkey: String,

    /// Absolute path to a GGUF model file.
    pub model: PathBuf,

    /// Language hint (ISO code). `None` lets the model detect, which costs accuracy on short
    /// utterances, so pin it if you always dictate in one language.
    #[serde(default)]
    pub language: Option<String>,

    /// Discard recordings shorter than this. Guards against an accidental tap producing a
    /// hallucinated transcript from a fraction of a second of noise.
    #[serde(default = "default_min_ms")]
    pub min_recording_ms: u64,

    /// Keep the model in memory between dictations.
    ///
    /// Loading is the slowest step by far, so this trades idle RAM for latency. Turn it off if the
    /// model is large and you dictate rarely.
    #[serde(default = "default_true")]
    pub keep_model_loaded: bool,

    /// Start with Windows (HKCU Run key). Asserted on every launch, so toggling it here and
    /// restarting is enough in either direction.
    #[serde(default)]
    pub autostart: bool,

    /// Pre-roll, in milliseconds. Zero keeps the mic closed until you press the key, which clips
    /// the first word if you start speaking immediately.
    ///
    /// Any value above zero keeps a rolling buffer of recent audio so the moment you press, the
    /// last `preroll_ms` are already captured and the first word survives. The cost is real and
    /// worth stating: the microphone stays open the whole time the app runs, so the Windows
    /// microphone indicator stays lit. Off by default because this is a privacy-first tool.
    #[serde(default)]
    pub preroll_ms: u64,

    /// Literal text replacements applied to every transcript, case-insensitively, on whole-word
    /// boundaries. For fixing names, jargon, and identifiers the model mishears.
    ///
    /// Each entry is `["heard", "wanted"]`, e.g. `["kubernetes", "Kubernetes"]` or
    /// `["k eight s", "k8s"]`. Deliberately literal, not fuzzy: a replacement you can read is a
    /// replacement that cannot silently corrupt the rest of the sentence.
    #[serde(default)]
    pub replacements: Vec<[String; 2]>,
}

fn default_min_ms() -> u64 {
    400
}

fn default_true() -> bool {
    true
}

impl Default for Config {
    fn default() -> Self {
        Self {
            hotkey: "CtrlLeft+WinLeft".to_string(),
            model: PathBuf::from("model.gguf"),
            language: Some("en".to_string()),
            min_recording_ms: default_min_ms(),
            keep_model_loaded: true,
            autostart: false,
            preroll_ms: 0,
            replacements: Vec::new(),
        }
    }
}

impl Config {
    /// Load config, writing a commented starter file if none exists.
    pub fn load_or_init(path: &Path) -> Result<Self> {
        if !path.exists() {
            std::fs::write(path, STARTER)
                .with_context(|| format!("writing starter config to {}", path.display()))?;
            anyhow::bail!(
                "No config found, so one was written to {}.\n\
                 Set `model` to a .gguf file and run again.",
                path.display()
            );
        }

        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        let config: Config =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;

        if !config.model.exists() {
            anyhow::bail!(
                "Model not found: {}\nSet `model` in {} to a .gguf file.",
                config.model.display(),
                path.display()
            );
        }
        Ok(config)
    }
}

const STARTER: &str = r#"# dictate configuration

# Hold this to record. Released, it transcribes and types.
# Modifier-only bindings work and are usually the most comfortable, because they
# cannot collide with an application shortcut. Examples:
#   "CtrlLeft+WinLeft"   "CtrlRight"   "AltRight"   "Ctrl+Space"   "F13"
hotkey = "CtrlLeft+WinLeft"

# Absolute path to a GGUF speech model.
# Handy's models live under ~/.cache/huggingface/hub if you already have it installed.
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

# Pre-roll in milliseconds. 0 opens the mic only while you hold the key.
# A value like 400 keeps recent audio buffered so the first word is never clipped,
# at the cost of holding the microphone open the whole time the app runs (the
# Windows mic indicator stays lit). Off by default; this is a privacy-first tool.
preroll_ms = 0

# Literal text fixes applied to every transcript, case-insensitive, whole-word.
# For names, jargon, and identifiers the model mishears. Examples:
#   replacements = [["kubernetes", "Kubernetes"], ["k eight s", "k8s"]]
replacements = []
"#;
