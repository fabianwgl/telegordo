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

    /// Sends TDLib's own log to a file, so it does not write over the TUI.
    /// stderr itself is left alone, so panic messages still reach the terminal.
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

    /// Editable markdown for a message (TDLib's inverse of `parseMarkdown`).
    /// Entities markdown cannot express stay in the result's `entities`.
    pub async fn markdown_of(&self, text: types::FormattedText) -> Result<types::FormattedText> {
        let enums::FormattedText::FormattedText(f) =
            functions::get_markdown_text(text, self.client_id)
                .await
                .map_err(td_err)?;
        Ok(f)
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

    /// Deletes a message, for everyone when `revoke` is set. If Telegram
    /// refuses the revoke for lack of rights, deletes it only for us.
    /// Returns whether it was deleted for everyone.
    pub async fn delete(&self, chat_id: i64, message_id: i64, revoke: bool) -> Result<bool> {
        match functions::delete_messages(chat_id, vec![message_id], revoke, self.client_id).await {
            Ok(()) => Ok(revoke),
            Err(e) if revoke && is_permission_error(&e) => {
                functions::delete_messages(chat_id, vec![message_id], false, self.client_id)
                    .await
                    .map_err(td_err)?;
                Ok(false)
            }
            Err(e) => Err(td_err(e)),
        }
    }

    /// Whether a message can be deleted for everyone and/or only for us.
    pub async fn delete_rights(&self, chat_id: i64, message_id: i64) -> Result<(bool, bool)> {
        let enums::MessageProperties::MessageProperties(p) =
            functions::get_message_properties(chat_id, message_id, self.client_id)
                .await
                .map_err(td_err)?;
        Ok((
            p.can_be_deleted_for_all_users,
            p.can_be_deleted_only_for_self,
        ))
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

/// Errors that mean "not allowed", as opposed to network or other failures.
fn is_permission_error(e: &types::Error) -> bool {
    e.code == 403 || e.message.contains("FORBIDDEN") || e.message.contains("rights")
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

/// Whether `key` looks like one `new_key` made: 32 bytes as 44 base64 chars.
fn valid_key(key: &str) -> bool {
    key.len() == 44
        && base64::engine::general_purpose::STANDARD
            .decode(key)
            .is_ok_and(|b| b.len() == 32)
}

const NO_KEYRING_HINT: &str = "no usable OS keyring; set `keyring = false` under [telegram] in \
     the config to keep the key in a 0600 file in the data dir instead";

/// Writes the key to a temporary file, syncs it and renames it into place,
/// so a crash or a full disk never leaves an empty or partial key file.
fn write_key_file(path: &Path, key: &str) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let tmp = path.with_extension("key.tmp");
    let _ = std::fs::remove_file(&tmp);
    let result = (|| -> Result<()> {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
            .with_context(|| format!("creating {}", tmp.display()))?;
        f.write_all(key.as_bytes())?;
        f.sync_all()?;
        std::fs::rename(&tmp, path).with_context(|| format!("creating {}", path.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
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
            let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER)
                .with_context(|| format!("opening keyring ({NO_KEYRING_HINT})"))?;
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
                Err(e) => Err(anyhow::anyhow!(e)
                    .context(format!("reading key from keyring ({NO_KEYRING_HINT})"))),
            }
        }
        KeyStore::File(path) => match std::fs::read_to_string(path) {
            Ok(key) => {
                let key = key.trim();
                if !valid_key(key) {
                    anyhow::bail!(
                        "the database key in {} is damaged (expected 44 base64 characters); \
                         restore it, or delete it and {} to log in again",
                        path.display(),
                        database_dir.display()
                    );
                }
                Ok(key.to_string())
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if db_exists {
                    return Err(missing());
                }
                if let Some(dir) = path.parent() {
                    std::fs::create_dir_all(dir)?;
                }
                let key = new_key();
                write_key_file(path, &key)?;
                Ok(key)
            }
            Err(e) => Err(anyhow::anyhow!(e).context(format!("reading {}", path.display()))),
        },
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("telegordo-key-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn key_file_is_created_atomically_and_read_back() {
        let dir = scratch("new");
        let path = dir.join("database.key");
        let store = KeyStore::File(path.clone());
        let key = database_key(&dir.join("tdlib"), &store).unwrap();
        assert!(valid_key(&key));
        assert!(!dir.join("database.key.tmp").exists());
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(database_key(&dir.join("tdlib"), &store).unwrap(), key);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_or_damaged_key_file_is_rejected() {
        let dir = scratch("bad");
        let path = dir.join("database.key");
        let store = KeyStore::File(path.clone());
        for bad in ["", "\n", "short", &"A".repeat(44)] {
            std::fs::write(&path, bad).unwrap();
            let e = database_key(&dir.join("tdlib"), &store).unwrap_err();
            assert!(e.to_string().contains("damaged"), "{bad:?}: {e}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unreadable_key_file_is_not_called_missing() {
        let dir = scratch("perm");
        let path = dir.join("database.key");
        std::fs::write(&path, new_key()).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        let tdlib = dir.join("tdlib");
        std::fs::create_dir_all(&tdlib).unwrap();
        std::fs::write(tdlib.join("td.binlog"), "x").unwrap();
        let result = database_key(&tdlib, &KeyStore::File(path.clone()));
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        // Root can read anything; the check only means something for a normal user.
        if let Err(e) = result {
            let msg = format!("{e:#}");
            assert!(!msg.contains("missing"), "{msg}");
            assert!(msg.contains("reading"), "{msg}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
