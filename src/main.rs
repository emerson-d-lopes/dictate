//! Personal push-to-talk dictation for Windows.
//!
//! Hold a key, speak, release. The text appears wherever you were typing.
//!
//! Deliberately has no settings UI. Configuration is a TOML file, feedback is a tray icon and a
//! small native bubble. That is the entire reason this is a few hundred lines rather than a few
//! tens of thousands.

// Release builds are windowless so autostart does not open a console at login.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod audio;
mod autostart;
mod config;
mod process;
mod ui;

use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, Result};
use handy_keys::{Hotkey, HotkeyManager, HotkeyState};
use transcribe_cpp::{Model, RunOptions, Session};

use crate::audio::Recording;
use crate::config::Config;

fn main() {
    if let Err(e) = run() {
        // In a windowless release build the console output goes nowhere, so a fatal error
        // must be a dialog or it is invisible.
        if cfg!(debug_assertions) {
            eprintln!("\nerror: {e:#}");
            eprintln!("\nPress Enter to exit.");
            let _ = std::io::stdin().read_line(&mut String::new());
        } else {
            error_box(&format!("{e:#}"));
        }
        std::process::exit(1);
    }
}

fn error_box(text: &str) {
    use windows::core::PCWSTR;
    use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK};
    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        MessageBoxW(
            None,
            PCWSTR(wide.as_ptr()),
            windows::core::w!("dictate"),
            MB_OK | MB_ICONERROR,
        );
    }
}

fn run() -> Result<()> {
    let config_path = config_path()?;
    let config = Config::load_or_init(&config_path)?;

    println!("dictate");
    println!("  config : {}", config_path.display());
    println!("  model  : {}", config.model.display());
    println!("  hotkey : {} (hold)", config.hotkey);

    // Backends must be initialised before any model loads, and the search directory is where the
    // ggml backend DLLs sit, which is next to this executable.
    transcribe_cpp::init_backends(exe_dir()?)
        .or_else(|_| transcribe_cpp::init_backends_default())
        .context("initialising transcription backends")?;

    autostart::apply(config.autostart).context("applying autostart setting")?;
    let ui = ui::Ui::start(&config.hotkey, &config_path)?;

    let mut engine = Engine::new(&config)?;

    let hotkey: Hotkey = config
        .hotkey
        .parse()
        .with_context(|| format!("'{}' is not a valid hotkey", config.hotkey))?;

    let manager = HotkeyManager::new().context("installing the keyboard hook")?;
    manager
        .register(hotkey)
        .with_context(|| format!("registering {}", config.hotkey))?;

    // Pre-roll mode keeps the mic open so the first word is never clipped. Off unless the config
    // asks for it, because an always-open mic lights the Windows indicator (see the config).
    let preroll = if config.preroll_ms > 0 {
        match audio::PrerollMic::new(config.preroll_ms) {
            Ok(m) => {
                println!("  preroll: {} ms (mic stays open)", config.preroll_ms);
                Some(m)
            }
            Err(e) => {
                eprintln!("pre-roll mic unavailable ({e:#}); opening on keypress instead");
                None
            }
        }
    } else {
        None
    };

    println!("\nready. hold {} and speak.\n", config.hotkey);

    // Exactly one of these is active per press, depending on pre-roll mode.
    let mut on_demand: Option<Recording> = None;
    let mut preroll_active = false;

    while let Ok(event) = manager.recv() {
        match event.state {
            HotkeyState::Pressed => {
                if on_demand.is_some() || preroll_active {
                    continue;
                }
                match &preroll {
                    Some(mic) => {
                        mic.begin();
                        preroll_active = true;
                        ui.recording();
                        print!("recording... ");
                        flush();
                    }
                    None => match Recording::start() {
                        Ok(r) => {
                            on_demand = Some(r);
                            ui.recording();
                            print!("recording... ");
                            flush();
                        }
                        Err(e) => {
                            ui.hide();
                            eprintln!("could not start recording: {e:#}");
                        }
                    },
                }
            }
            HotkeyState::Released => {
                let samples = match &preroll {
                    Some(mic) if preroll_active => {
                        preroll_active = false;
                        mic.end()
                    }
                    _ => match on_demand.take() {
                        Some(r) => r.finish(),
                        None => continue,
                    },
                };
                ui.transcribing();
                let result = samples.and_then(|s| deliver(s, &config, &mut engine));
                ui.hide();
                if let Err(e) = result {
                    eprintln!("failed: {e:#}");
                }
            }
        }
    }

    Ok(())
}

/// Everything that happens between releasing the key and the text appearing.
fn deliver(samples: Vec<f32>, config: &Config, engine: &mut Engine) -> Result<()> {
    let started = Instant::now();

    let seconds = samples.len() as f32 / audio::TARGET_RATE as f32;
    if (seconds * 1000.0) < config.min_recording_ms as f32 {
        println!("too short, ignored");
        return Ok(());
    }

    // Rejecting near-silence before inference is the cheapest guard against a hallucinated
    // transcript, and it also catches a muted or wrongly-selected microphone.
    if audio::peak(&samples) < 0.005 {
        println!("silence, ignored");
        return Ok(());
    }

    // Trim the dead air before and after speech: faster, and it keeps the model from hallucinating
    // at the quiet edges.
    let samples = process::trim_silence(&samples, audio::TARGET_RATE);

    // Capture the target after the key is released: whatever is focused now is what the user
    // meant to type into.
    let target =
        win_text_inject::Target::foreground().context("no foreground window to type into")?;

    // Transcription can fail transiently (a GPU backend hiccup), and the audio only exists in RAM
    // right now, so retry once before giving up. Nothing is written to disk, so this is the only
    // recovery window there is.
    let raw = match engine.transcribe(&samples, config) {
        Ok(t) => t,
        Err(first) => {
            eprintln!("transcription failed ({first:#}), retrying once");
            engine
                .transcribe(&samples, config)
                .context("transcription failed twice; the recording is lost")?
        }
    };

    // Apply the user's literal fixes, then trim. Order matters: a rule may add or remove edge
    // whitespace.
    let fixed = process::apply_replacements(raw.trim(), &config.replacements);
    let text = fixed.trim();
    if text.is_empty() {
        println!("nothing recognised");
        return Ok(());
    }

    // From here the text is recognised, so it must never be lost to a delivery failure. If
    // injection cannot type it, leave it on the clipboard instead. inject() already does this for
    // elevated targets; this covers every other injection error the same way.
    let elapsed = started.elapsed().as_millis();
    match win_text_inject::inject(&target, text, Default::default()) {
        Ok(win_text_inject::Outcome::ClipboardOnly(reason)) => {
            println!("[{elapsed} ms] on clipboard only ({reason:?}), press Ctrl+V");
        }
        Ok(_) => println!("[{elapsed} ms] {text}"),
        Err(e) => {
            // Last resort: put it on the clipboard so a Ctrl+V recovers it.
            match win_text_inject::clipboard::set_text_private(text) {
                Ok(()) => println!("[{elapsed} ms] could not type ({e}); on clipboard, press Ctrl+V"),
                Err(clip) => {
                    // Both paths failed. Print it so at least it is visible to copy by hand.
                    eprintln!("could not type or copy ({e}; {clip}). transcript:\n{text}");
                }
            }
        }
    }
    Ok(())
}

/// Holds the loaded model so repeated dictations do not pay for loading it.
struct Engine {
    model_path: PathBuf,
    session: Option<Session>,
    keep_loaded: bool,
}

impl Engine {
    fn new(config: &Config) -> Result<Self> {
        let mut engine = Self {
            model_path: config.model.clone(),
            session: None,
            keep_loaded: config.keep_model_loaded,
        };
        if config.keep_model_loaded {
            print!("loading model... ");
            flush();
            engine.session = Some(engine.load()?);
            println!("ok");
        }
        Ok(engine)
    }

    fn load(&self) -> Result<Session> {
        Model::load(&self.model_path)
            .with_context(|| format!("loading {}", self.model_path.display()))?
            .session()
            .context("creating a transcription session")
    }

    fn transcribe(&mut self, samples: &[f32], config: &Config) -> Result<String> {
        let options = RunOptions {
            language: config.language.clone(),
            ..Default::default()
        };

        if self.keep_loaded {
            if self.session.is_none() {
                self.session = Some(self.load()?);
            }
            let session = self.session.as_mut().expect("just ensured");
            return Ok(session.run(samples, &options).context("transcribing")?.text);
        }

        let mut session = self.load()?;
        Ok(session.run(samples, &options).context("transcribing")?.text)
    }
}

fn exe_dir() -> Result<PathBuf> {
    Ok(std::env::current_exe()
        .context("locating the executable")?
        .parent()
        .context("executable has no parent directory")?
        .to_path_buf())
}

fn config_path() -> Result<PathBuf> {
    Ok(exe_dir()?.join("dictate.toml"))
}

fn flush() {
    use std::io::Write;
    let _ = std::io::stdout().flush();
}
