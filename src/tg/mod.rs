//! TDLib client: one blocking receive thread plus async request helpers.

pub mod store;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};
use base64::Engine;
use tdlib_rs::enums::{self, ChatList, InputMessageContent, MessageTopic, Update};
use tdlib_rs::functions;
use tdlib_rs::types;
use tokio::sync::mpsc::UnboundedSender;

const KEYRING_SERVICE: &str = "telegordo";
const KEYRING_USER: &str = "database_encryption_key";

/// Where a message list comes from: a whole chat, or one forum topic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct View {
    pub chat_id: i64,
    pub topic: Option<i32>,
}

impl View {
    pub fn message_topic(self) -> Option<MessageTopic> {
        self.topic
            .map(|t| MessageTopic::Forum(types::MessageTopicForum { forum_topic_id: t }))
    }
}

#[derive(Clone)]
pub struct Tg {
    pub client_id: i32,
    stop: Arc<AtomicBool>,
}

impl Tg {
    /// Creates the TDLib client and starts forwarding updates to `tx`.
    /// Responses to requests are only delivered while the receive loop runs,
    /// so it starts before anything is sent.
    pub fn start(tx: UnboundedSender<Update>) -> Self {
        let client_id = tdlib_rs::create_client();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = stop.clone();
        std::thread::Builder::new()
            .name("tdlib-receive".into())
            .spawn(move || {
                while !stop_flag.load(Ordering::Relaxed) {
                    if let Some((update, _client)) = tdlib_rs::receive() {
                        let closed = matches!(
                            &update,
                            Update::AuthorizationState(s)
                                if matches!(s.authorization_state, enums::AuthorizationState::Closed)
                        );
                        if tx.send(update).is_err() || closed {
                            break;
                        }
                    }
                }
            })
            .expect("spawn tdlib receive thread");
        Self { client_id, stop }
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    /// Sends TDLib's own log to a file and redirects stderr there too,
    /// so nothing writes over the TUI.
    pub async fn setup_logging(&self, path: &Path, verbosity: i32) -> Result<()> {
        functions::set_log_verbosity_level(verbosity, self.client_id)
            .await
            .map_err(td_err)?;
        functions::set_log_stream(
            enums::LogStream::File(types::LogStreamFile {
                path: path.to_string_lossy().into_owned(),
                max_file_size: 10 * 1024 * 1024,
                redirect_stderr: false,
            }),
            self.client_id,
        )
        .await
        .map_err(td_err)?;
        Ok(())
    }

    pub async fn set_parameters(&self, p: &Params) -> Result<()> {
        functions::set_tdlib_parameters(
            p.use_test_dc,
            p.database_dir.to_string_lossy().into_owned(),
            p.files_dir.to_string_lossy().into_owned(),
            p.encryption_key.clone(),
            true,
            true,
            true,
            false,
            p.api_id,
            p.api_hash.clone(),
            "en".into(),
            "Desktop".into(),
            std::env::consts::OS.into(),
            env!("CARGO_PKG_VERSION").into(),
            self.client_id,
        )
        .await
        .map_err(td_err)
    }

    pub async fn load_all_chats(&self, list: ChatList) {
        // TDLib answers 404 once every chat of the list is loaded.
        loop {
            match functions::load_chats(Some(list.clone()), 100, self.client_id).await {
                Ok(()) => continue,
                Err(e) if e.code == 404 => break,
                Err(e) => {
                    tracing::warn!("load_chats {list:?}: {} {}", e.code, e.message);
                    break;
                }
            }
        }
    }

    pub async fn history(
        &self,
        view: View,
        from_message_id: i64,
        limit: i32,
    ) -> Result<Vec<types::Message>> {
        let res = match view.topic {
            Some(topic) => {
                functions::get_forum_topic_history(
                    view.chat_id,
                    topic,
                    from_message_id,
                    0,
                    limit,
                    self.client_id,
                )
                .await
            }
            None => {
                functions::get_chat_history(
                    view.chat_id,
                    from_message_id,
                    0,
                    limit,
                    false,
                    self.client_id,
                )
                .await
            }
        };
        let enums::Messages::Messages(m) = res.map_err(td_err)?;
        Ok(m.messages.into_iter().flatten().collect())
    }

    /// Messages around `message_id` (half before, half after).
    pub async fn history_around(
        &self,
        chat_id: i64,
        message_id: i64,
        limit: i32,
    ) -> Result<Vec<types::Message>> {
        let res = functions::get_chat_history(
            chat_id,
            message_id,
            -(limit / 2),
            limit,
            false,
            self.client_id,
        )
        .await;
        let enums::Messages::Messages(m) = res.map_err(td_err)?;
        Ok(m.messages.into_iter().flatten().collect())
    }

    pub async fn forum_topics(&self, chat_id: i64) -> Result<Vec<types::ForumTopic>> {
        let mut out = Vec::new();
        let (mut date, mut msg, mut topic) = (0, 0, 0);
        loop {
            let enums::ForumTopics::ForumTopics(page) = functions::get_forum_topics(
                chat_id,
                String::new(),
                date,
                msg,
                topic,
                100,
                self.client_id,
            )
            .await
            .map_err(td_err)?;
            let n = page.topics.len();
            out.extend(page.topics);
            if n == 0
                || (page.next_offset_date == 0
                    && page.next_offset_message_id == 0
                    && page.next_offset_forum_topic_id == 0)
            {
                break;
            }
            date = page.next_offset_date;
            msg = page.next_offset_message_id;
            topic = page.next_offset_forum_topic_id;
            if out.len() >= page.total_count as usize {
                break;
            }
        }
        Ok(out)
    }

    pub async fn formatted(&self, text: String, markdown: bool) -> types::FormattedText {
        let plain = types::FormattedText {
            text,
            entities: vec![],
        };
        if !markdown {
            return plain;
        }
        match functions::parse_markdown(plain.clone(), self.client_id).await {
            Ok(enums::FormattedText::FormattedText(f)) => f,
            Err(e) => {
                tracing::warn!("parse_markdown: {}", e.message);
                plain
            }
        }
    }

    pub async fn send_text(
        &self,
        view: View,
        reply_to: Option<i64>,
        text: types::FormattedText,
    ) -> Result<()> {
        let content = text_content(text);
        let reply_to = reply_to.map(|id| {
            enums::InputMessageReplyTo::Message(types::InputMessageReplyToMessage {
                message_id: id,
                quote: None,
                checklist_task_id: 0,
            })
        });
        functions::send_message(
            view.chat_id,
            view.message_topic(),
            reply_to,
            None,
            content,
            self.client_id,
        )
        .await
        .map_err(td_err)?;
        Ok(())
    }

    pub async fn edit_text(
        &self,
        chat_id: i64,
        message_id: i64,
        text: types::FormattedText,
    ) -> Result<()> {
        functions::edit_message_text(chat_id, message_id, text_content(text), self.client_id)
            .await
            .map_err(td_err)?;
        Ok(())
    }

    /// Deletes for everyone when allowed, otherwise only for us.
    pub async fn delete(&self, chat_id: i64, message_id: i64) -> Result<()> {
        if functions::delete_messages(chat_id, vec![message_id], true, self.client_id)
            .await
            .is_ok()
        {
            return Ok(());
        }
        functions::delete_messages(chat_id, vec![message_id], false, self.client_id)
            .await
            .map_err(td_err)
    }

    pub async fn download(
        &self,
        file_id: i32,
        priority: i32,
        synchronous: bool,
    ) -> Result<types::File> {
        let enums::File::File(f) =
            functions::download_file(file_id, priority, 0, 0, synchronous, self.client_id)
                .await
                .map_err(td_err)?;
        Ok(f)
    }
}

fn text_content(text: types::FormattedText) -> InputMessageContent {
    InputMessageContent::InputMessageText(types::InputMessageText {
        text,
        link_preview_options: None,
        clear_draft: true,
    })
}

pub fn td_err(e: types::Error) -> anyhow::Error {
    anyhow::anyhow!("telegram error {}: {}", e.code, e.message)
}

/// Not `Debug`: holds the database key.
#[derive(Clone)]
pub struct Params {
    pub use_test_dc: bool,
    pub database_dir: PathBuf,
    pub files_dir: PathBuf,
    pub encryption_key: String,
    pub api_id: i32,
    pub api_hash: String,
    pub key_store: KeyStore,
}

/// Where the database key lives.
#[derive(Debug, Clone)]
pub enum KeyStore {
    /// OS keyring (macOS Keychain, Secret Service on Linux), like discordo's token.
    Keyring,
    /// A 0600 file next to the database, for systems without a keyring.
    File(PathBuf),
}

fn new_key() -> String {
    let bytes: [u8; 32] = rand::random();
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// The TDLib database is encrypted with a random key from `store`.
pub fn database_key(database_dir: &Path, store: &KeyStore) -> Result<String> {
    let missing = || {
        anyhow::anyhow!(
            "the session database at {} exists but its key is missing; delete that folder to log in again",
            database_dir.display()
        )
    };
    let db_exists = database_dir.join("td.binlog").exists();
    match store {
        KeyStore::Keyring => {
            let entry =
                keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER).context("opening keyring")?;
            match entry.get_password() {
                Ok(key) => Ok(key),
                Err(keyring::Error::NoEntry) => {
                    if db_exists {
                        return Err(missing());
                    }
                    let key = new_key();
                    entry.set_password(&key).context("saving key to keyring")?;
                    Ok(key)
                }
                Err(e) => Err(anyhow::anyhow!(e).context("reading key from keyring")),
            }
        }
        KeyStore::File(path) => {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            if let Ok(key) = std::fs::read_to_string(path) {
                return Ok(key.trim().to_string());
            }
            if db_exists {
                return Err(missing());
            }
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let key = new_key();
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(path)
                .with_context(|| format!("creating {}", path.display()))?;
            f.write_all(key.as_bytes())?;
            Ok(key)
        }
    }
}

pub fn forget_database_key(store: &KeyStore) {
    match store {
        KeyStore::Keyring => {
            if let Ok(entry) = keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER)
                && let Err(e) = entry.delete_credential()
            {
                tracing::warn!("keyring delete: {e}");
            }
        }
        KeyStore::File(path) => {
            let _ = std::fs::remove_file(path);
        }
    }
}
