#![recursion_limit = "256"]
//! telegordo: a discordo-style Telegram client for the terminal.

mod app;
mod config;
mod keymap;
mod notify;
mod outbox;
#[cfg(test)]
mod screenshots;
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

use crate::app::{App, Interrupt, Signals};
use crate::config::Config;
use crate::keymap::Keymap;
use crate::tg::{KeyStore, Params, Tg};
use crate::ui::image::Images;

#[derive(Parser)]
#[command(version, about = "A discordo-style Telegram client for the terminal")]
struct Cli {
    /// Config file to use instead of the lookup ($XDG_CONFIG_HOME/telegordo,
    /// ~/.config/telegordo, then the platform config dir); must exist
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
    let explicit = cli.config_path.is_some();
    let config_path = match cli.config_path {
        Some(p) => p,
        None => config::default_config_path()?,
    };
    let cfg = Config::load(&config_path, explicit)?;
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
    // TDLib logs to stderr until its log goes to a file; keep that off the
    // terminal. The receive thread logs too, so stay silent until then.
    let silence = StderrRedirect::to_null();
    let tg = Tg::start(td_tx);
    tg.setup_logging(&cache.join("tdlib.log"), 1).await?;
    drop(silence);

    // Before the terminal is set up, so a failure leaves it untouched.
    let mut signals = Signals::new().context("installing signal handlers")?;
    install_panic_hook(cfg.mouse);
    let mut terminal =
        ratatui::try_init().context("telegordo needs an interactive terminal (a TTY)")?;
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
    let (keys, key_warnings) = Keymap::build(&cfg.keys, enhanced);
    let warnings: Vec<String> = cfg.warnings.iter().cloned().chain(key_warnings).collect();
    for w in &warnings {
        tracing::warn!("config: {w}");
    }

    let mut app = App::new(cfg, keys, tg.clone(), params, images, app_tx);
    if let Some(w) = warnings.first() {
        let more = match warnings.len() {
            1 => String::new(),
            n => format!(" (+{} more in the log)", n - 1),
        };
        app.set_status(format!("config: {w}{more}"));
    }

    let result = loop {
        match app
            .run(&mut terminal, &mut td_rx, &mut app_rx, &mut signals)
            .await
        {
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
                // Not `ratatui::init()`: that would stack another panic hook.
                crossterm::terminal::enable_raw_mode()?;
                execute!(std::io::stdout(), crossterm::terminal::EnterAlternateScreen)?;
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

/// Opens the app log, replacing it, readable by the owner only.
fn open_log(path: &Path) -> Result<std::fs::File> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("creating log {}", path.display()))?;
    // `mode` only applies to new files; fix up a log left by an older version.
    // Only regular files: /dev/null, a FIFO or a tty must not be chmodded.
    if file.metadata().is_ok_and(|m| m.is_file()) {
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("chmod {}", path.display()))?;
    }
    Ok(file)
}

fn init_logging(path: &Path, level: &str) -> Result<()> {
    let file = open_log(path)?;
    let filter = tracing_subscriber::EnvFilter::try_new(format!("telegordo={level},warn"))
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_writer(std::sync::Mutex::new(file))
        .with_ansi(false)
        .with_env_filter(filter)
        .init();
    Ok(())
}

/// Points fd 2 somewhere else until dropped, then restores it.
struct StderrRedirect {
    saved: Option<libc::c_int>,
}

impl StderrRedirect {
    fn to_null() -> Self {
        Self::to(c"/dev/null")
    }

    fn to(path: &std::ffi::CStr) -> Self {
        // SAFETY: plain fd juggling on fd 2; `Drop` puts the saved fd back.
        unsafe {
            let target = libc::open(
                path.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_APPEND,
                0o600,
            );
            if target < 0 {
                return Self { saved: None };
            }
            let saved = libc::dup(2);
            if saved < 0 {
                libc::close(target);
                return Self { saved: None };
            }
            libc::dup2(target, 2);
            libc::close(target);
            Self { saved: Some(saved) }
        }
    }
}

impl Drop for StderrRedirect {
    fn drop(&mut self) {
        if let Some(saved) = self.saved.take() {
            // SAFETY: `saved` is the fd 2 we duplicated in `to`.
            unsafe {
                libc::dup2(saved, 2);
                libc::close(saved);
            }
        }
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
    // Private: the draft may be a private message. A random name opened with
    // O_EXCL never follows a symlink or reuses a file someone else made in a
    // shared /tmp. The .md suffix makes editors pick markdown mode.
    let (path, mut f) = loop {
        let n: u64 = rand::random();
        let path = std::env::temp_dir().join(format!("telegordo-{n:016x}.md"));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(f) => break (path, f),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e).context("creating draft file"),
        }
    };
    let written = f.write_all(text.as_bytes());
    drop(f);
    if let Err(e) = written {
        let _ = std::fs::remove_file(&path);
        return Err(e).context("writing draft file");
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("telegordo-main-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn log_file_is_private_even_if_it_existed() {
        let dir = scratch("log");
        let path = dir.join("telegordo.log");
        std::fs::write(&path, "old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        drop(open_log(&path).unwrap());
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "", "truncated");
        let _ = std::fs::remove_dir_all(&dir);
        // Devices are used as they are, not chmodded (that fails with EPERM).
        open_log(Path::new("/dev/null")).expect("/dev/null works as a log");
    }

    #[test]
    fn editor_draft_never_touches_the_old_predictable_path() {
        let old = std::env::temp_dir().join(format!("telegordo-{}.md", std::process::id()));
        std::fs::write(&old, "victim").unwrap();
        // `true` leaves the file as written, so the text comes back unchanged.
        assert_eq!(run_editor("true", "hello").unwrap(), "hello");
        assert_eq!(std::fs::read_to_string(&old).unwrap(), "victim");
        let _ = std::fs::remove_file(&old);
    }

    #[test]
    fn stderr_redirect_is_undone_on_drop() {
        let dir = scratch("stderr");
        let cpath = |p: &Path| std::ffi::CString::new(p.to_str().unwrap()).unwrap();
        let (outer, inner) = (dir.join("outer.txt"), dir.join("inner.txt"));
        let write2 = |s: &str| unsafe {
            libc::write(2, s.as_ptr().cast(), s.len());
        };
        {
            let _outer = StderrRedirect::to(&cpath(&outer));
            {
                let _inner = StderrRedirect::to(&cpath(&inner));
                write2("while-inner\n");
            }
            // The inner guard put the outer target back.
            write2("after-inner\n");
        }
        let inner = std::fs::read_to_string(&inner).unwrap();
        let outer = std::fs::read_to_string(&outer).unwrap();
        assert!(inner.contains("while-inner") && !inner.contains("after-inner"));
        assert!(outer.contains("after-inner") && !outer.contains("while-inner"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
