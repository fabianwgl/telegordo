#![recursion_limit = "256"]
//! telegordo: a discordo-style Telegram client for the terminal.

mod app;
mod config;
mod keymap;
mod notify;
#[cfg(test)]
mod testutil;
mod tg;
mod ui;

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Parser;
use crossterm::event::{
    DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
    EnableFocusChange, EnableMouseCapture, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::execute;

use crate::app::{App, Interrupt};
use crate::config::Config;
use crate::keymap::Keymap;
use crate::tg::{KeyStore, Params, Tg};
use crate::ui::image::Images;

#[derive(Parser)]
#[command(version, about = "A discordo-style Telegram client for the terminal")]
struct Cli {
    /// Config file [default: platform config dir/telegordo/config.toml]
    #[arg(long)]
    config_path: Option<PathBuf>,
    /// Log file, replaced on each run [default: platform cache dir/telegordo/telegordo.log]
    #[arg(long)]
    log_path: Option<PathBuf>,
    /// Session and file cache directory [default: platform data dir/telegordo]
    #[arg(long)]
    data_dir: Option<PathBuf>,
    /// debug, info, warn or error
    #[arg(long, default_value = "info")]
    log_level: String,
}

const NO_API_ID: &str = "\
telegordo needs your own Telegram API credentials.

1. Log in at https://my.telegram.org and open \"API development tools\".
2. Create an app (any name), then either export:

     export TELEGORDO_API_ID=12345
     export TELEGORDO_API_HASH=0123456789abcdef0123456789abcdef

   or put them in the config file:

     [telegram]
     api_id = 12345
     api_hash = \"0123456789abcdef0123456789abcdef\"
";

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let config_path = match cli.config_path {
        Some(p) => p,
        None => config::default_config_path()?,
    };
    let cfg = Config::load(&config_path)?;
    let dirs = config::project_dirs()?;
    let cache = dirs.cache_dir().to_path_buf();
    std::fs::create_dir_all(&cache).with_context(|| format!("creating {}", cache.display()))?;
    let log_path = cli.log_path.unwrap_or_else(|| cache.join("telegordo.log"));
    init_logging(&log_path, &cli.log_level)?;

    let Some((api_id, api_hash)) = cfg.api_credentials() else {
        eprint!("{NO_API_ID}");
        if config_path.exists() {
            eprintln!(
                "\nRead {} but it has no complete [telegram] api_id + api_hash.",
                config_path.display()
            );
        } else {
            eprintln!("\nNo config file found. Looked in:");
            for p in config::config_candidates().unwrap_or_default() {
                eprintln!("  {}", p.display());
            }
        }
        std::process::exit(2);
    };

    let data = cli
        .data_dir
        .unwrap_or_else(|| dirs.data_dir().to_path_buf());
    let database_dir = data.join("tdlib");
    let key_store = if cfg.telegram.keyring {
        KeyStore::Keyring
    } else {
        KeyStore::File(data.join("database.key"))
    };
    let params = Params {
        use_test_dc: cfg.telegram.use_test_dc,
        encryption_key: tg::database_key(&database_dir, &key_store)?,
        key_store,
        database_dir,
        files_dir: data.join("files"),
        api_id,
        api_hash,
    };

    let (td_tx, mut td_rx) = tokio::sync::mpsc::unbounded_channel();
    let (app_tx, mut app_rx) = tokio::sync::mpsc::unbounded_channel();
    // TDLib logs to stderr until told otherwise; keep that off the terminal.
    let tg = with_stderr_silenced(|| Tg::start(td_tx));
    tg.setup_logging(&cache.join("tdlib.log"), 1).await?;

    install_panic_hook(cfg.mouse);
    let mut terminal = ratatui::init();
    let enhanced = enter_extras(cfg.mouse);
    let images = Images::new(&cfg.images);
    tracing::info!(
        "kitty keyboard protocol: {}",
        if enhanced {
            "on"
        } else {
            "off (alt+ fallbacks)"
        }
    );
    let (keys, warnings) = Keymap::build(&cfg.keys, enhanced);
    for w in &warnings {
        tracing::warn!("config: {w}");
    }

    let mut app = App::new(cfg, keys, tg.clone(), params, images, app_tx);
    if let Some(w) = warnings.first() {
        app.set_status(format!("config: {w}"));
    }

    let result = loop {
        match app.run(&mut terminal, &mut td_rx, &mut app_rx).await {
            Ok(None) => break Ok(()),
            Ok(Some(interrupt)) => {
                leave_extras(app.cfg.mouse, enhanced);
                ratatui::restore();
                match interrupt {
                    Interrupt::Editor => {
                        let text = app.composer_text();
                        match run_editor(&app.cfg.editor, &text) {
                            Ok(t) => app.set_composer_text(&t),
                            Err(e) => app.set_status(format!("editor: {e:#}")),
                        }
                    }
                    Interrupt::Suspend => {
                        // Stops here until the shell resumes us with `fg`.
                        unsafe {
                            libc::raise(libc::SIGTSTP);
                        }
                    }
                }
                terminal = ratatui::init();
                enter_extras(app.cfg.mouse);
                terminal.clear()?;
                app.dirty = true;
            }
            Err(e) => break Err(e),
        }
    };

    leave_extras(app.cfg.mouse, enhanced);
    ratatui::restore();
    tg.stop();
    result
}

fn init_logging(path: &Path, level: &str) -> Result<()> {
    let file =
        std::fs::File::create(path).with_context(|| format!("creating log {}", path.display()))?;
    let filter = tracing_subscriber::EnvFilter::try_new(format!("telegordo={level},warn"))
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_writer(std::sync::Mutex::new(file))
        .with_ansi(false)
        .with_env_filter(filter)
        .init();
    Ok(())
}

fn with_stderr_silenced<T>(f: impl FnOnce() -> T) -> T {
    // SAFETY: plain fd juggling on fd 2, restored before returning.
    unsafe {
        let saved = libc::dup(2);
        let null = libc::open(c"/dev/null".as_ptr(), libc::O_WRONLY);
        if saved < 0 || null < 0 {
            return f();
        }
        libc::dup2(null, 2);
        libc::close(null);
        let out = f();
        libc::dup2(saved, 2);
        libc::close(saved);
        out
    }
}

/// Mouse, focus events, bracketed paste and (when supported) the kitty
/// keyboard protocol. Returns whether the protocol is on.
fn enter_extras(mouse: bool) -> bool {
    let mut out = std::io::stdout();
    let enhanced = matches!(
        crossterm::terminal::supports_keyboard_enhancement(),
        Ok(true)
    );
    let _ = execute!(out, EnableFocusChange, EnableBracketedPaste);
    if mouse {
        let _ = execute!(out, EnableMouseCapture);
    }
    if enhanced {
        let _ = execute!(
            out,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        );
    }
    let _ = out.flush();
    enhanced
}

fn leave_extras(mouse: bool, enhanced: bool) {
    let mut out = std::io::stdout();
    if enhanced {
        let _ = execute!(out, PopKeyboardEnhancementFlags);
    }
    if mouse {
        let _ = execute!(out, DisableMouseCapture);
    }
    let _ = execute!(out, DisableFocusChange, DisableBracketedPaste);
    let _ = out.flush();
}

fn install_panic_hook(mouse: bool) {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // Pop the protocol unconditionally: harmless if it was never pushed.
        let mut out = std::io::stdout();
        let _ = execute!(out, PopKeyboardEnhancementFlags);
        leave_extras(mouse, false);
        prev(info);
    }));
}

/// Edits `text` in `$VISUAL` / `$EDITOR` (or the configured editor).
fn run_editor(configured: &str, text: &str) -> Result<String> {
    use std::os::unix::fs::OpenOptionsExt;
    let cmd = if configured == "default" {
        std::env::var("VISUAL")
            .or_else(|_| std::env::var("EDITOR"))
            .unwrap_or_else(|_| "vi".into())
    } else {
        configured.to_string()
    };
    let path = std::env::temp_dir().join(format!("telegordo-{}.md", std::process::id()));
    {
        // Private: the draft may be a private message.
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)?;
        f.write_all(text.as_bytes())?;
    }
    let mut parts = cmd.split_whitespace();
    let program = parts.next().context("empty editor command")?;
    let status = std::process::Command::new(program)
        .args(parts)
        .arg(&path)
        .status();
    let edited = std::fs::read_to_string(&path);
    let _ = std::fs::remove_file(&path);
    let status = status.with_context(|| format!("running {program}"))?;
    if !status.success() {
        anyhow::bail!("{program} exited with {status}");
    }
    Ok(edited?)
}
