//! Application state, event loop and action handling.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyEvent, KeyEventKind, MouseButton, MouseEventKind};
use futures::StreamExt;
use ratatui::DefaultTerminal;
use ratatui::layout::Rect;
use ratatui_image::protocol::Protocol;
use ratatui_textarea::TextArea;
use tdlib_rs::enums::{self, AuthorizationState, MessageContent, Update};
use tdlib_rs::functions;
use tdlib_rs::types::{File, ForumTopic, Message};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::config::Config;
use crate::keymap::{
    ComposerAction, GlobalAction, Keymap, MessagesAction, PickerAction, TreeAction,
};
use crate::outbox::Body;
use crate::outbox::{self, Op};
use crate::tg::store::{ListKey, Store, message_in_topic};
use crate::tg::{Params, Tg, View};
use crate::ui::chats_tree::{self, NodeId, TreeState};
use crate::ui::composer::{self, Composer, Draft, EditOrigin, Mode};
use crate::ui::image::{self, Images, Wanted};
use crate::ui::messages::{self, MessagesState};
use crate::ui::picker::ChatPicker;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Tree,
    Messages,
    Composer,
}

pub enum Modal {
    None,
    Picker(ChatPicker),
    /// Key list, scrolled `scroll` lines down.
    Help {
        scroll: u16,
    },
    /// `rights` is None until `getMessageProperties` answers.
    ConfirmDelete {
        chat_id: i64,
        message_id: i64,
        rights: Option<DeleteRights>,
    },
    ConfirmLogout,
    /// A downloaded file that can run code.
    ConfirmOpen {
        path: String,
    },
}

/// What `getMessageProperties` allows for a delete.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeleteRights {
    pub for_everyone: bool,
    pub only_for_self: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Auth {
    Starting,
    Phone,
    Qr(String),
    Code(String),
    Password(String),
    Email,
    EmailCode,
    Registration,
    Ready,
    Closing,
}

#[derive(Debug, Clone)]
pub enum FilePurpose {
    Image(Wanted),
    Open,
    Save(PathBuf),
}

pub enum AppEvent {
    History {
        view: View,
        from: i64,
        result: Result<Vec<Message>, String>,
    },
    Around {
        view: View,
        target: i64,
        result: Result<Vec<Message>, String>,
    },
    Topics {
        chat_id: i64,
        result: Result<Vec<ForumTopic>, String>,
    },
    File {
        purpose: FilePurpose,
        result: Result<File, String>,
    },
    ImageReady {
        wanted: Wanted,
        result: Result<Protocol, String>,
    },
    DeleteRights {
        chat_id: i64,
        message_id: i64,
        result: Result<DeleteRights, String>,
    },
    Status(String),
    AuthError(String),
    Me(i64),
    /// `logOut` failed; the session stays.
    LogOutFailed(String),
    /// A backwards page from the newer end of a history gap.
    GapPage {
        chat_id: i64,
        newer_end: i64,
        result: Result<Vec<Message>, String>,
    },
    /// Default `mute_for` of a kind of chat, fetched at login.
    ScopeMute {
        scope: enums::NotificationSettingsScope,
        mute_for: i32,
    },
    /// A send or edit failed; `draft` is what the composer held.
    SendFailed {
        view: View,
        draft: Draft,
        error: String,
    },
    /// Markdown for a formatted message the user started editing.
    EditReady {
        view: View,
        message_id: i64,
        original: tdlib_rs::types::FormattedText,
        result: Result<tdlib_rs::types::FormattedText, String>,
    },
}

/// SIGTERM and SIGHUP, registered once for the whole run so a signal that
/// arrives while the editor runs is seen when the loop resumes.
pub struct Signals {
    term: tokio::signal::unix::Signal,
    hup: tokio::signal::unix::Signal,
}

impl Signals {
    pub fn new() -> std::io::Result<Self> {
        use tokio::signal::unix::{SignalKind, signal};
        Ok(Self {
            term: signal(SignalKind::terminate())?,
            hup: signal(SignalKind::hangup())?,
        })
    }
}

/// Something the loop must do outside of normal event handling.
pub enum Interrupt {
    Editor,
    Suspend,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct Rects {
    pub tree: Rect,
    pub messages: Rect,
    pub composer: Rect,
    /// Inner message area and its first visible layout line.
    pub messages_inner: Rect,
    pub messages_top: usize,
    /// Blank rows above the first line when the history is shorter than the pane.
    pub messages_pad: u16,
    pub tree_inner: Rect,
}

pub struct App {
    pub cfg: Config,
    pub keys: Keymap,
    pub tg: Tg,
    pub params: Params,
    pub store: Store,
    pub auth: Auth,
    pub login_input: TextArea<'static>,
    pub login_error: Option<String>,
    pub login_busy: bool,
    pub focus: Focus,
    pub sidebar: bool,
    pub tree: TreeState,
    pub msgs: MessagesState,
    pub composer: Composer,
    pub modal: Modal,
    pub current: Option<View>,
    pub images: Images,
    pub status: Option<(String, Instant)>,
    pub terminal_focused: bool,
    pub rects: Rects,
    /// Message id ranges from the last draw, for mouse clicks.
    pub last_ranges: Vec<(i64, usize, usize)>,
    /// Images the last draw wanted.
    pub wanted: Vec<Wanted>,
    /// Newer ends of the history gaps the last draw showed.
    pub wanted_gaps: Vec<i64>,
    pub tx: UnboundedSender<AppEvent>,
    /// Ordered queue of user-initiated TDLib requests.
    pub outbox: UnboundedSender<Op>,
    pub quit_at: Option<Instant>,
    pub logging_out: bool,
    /// TDLib reported `authorizationStateLoggingOut`: the server accepted it.
    server_logging_out: bool,
    pub dirty: bool,
    /// Connection banner, None when connected.
    pub connection: Option<&'static str>,
    tg_closed: bool,
    pending_interrupt: Option<Interrupt>,
    /// Kept for the whole run: on X11/Wayland the copied text lives only as
    /// long as its owner.
    clipboard: Option<arboard::Clipboard>,
    /// Unsent composer text per view, kept while another chat is open.
    drafts: HashMap<View, Draft>,
    /// Whether the terminal ever reported focus. Without focus events
    /// (tmux without `focus-events on`) the window counts as unfocused for
    /// notifications.
    focus_events_seen: bool,
    ticks: u64,
}

/// Messages kept for a chat that is not open: none needed for a chat that
/// was never opened beyond the last one, a page or so for one that was.
const KEEP_UNOPENED: usize = 1;
const KEEP_BACKGROUND: usize = 500;

fn login_area(mask: bool) -> TextArea<'static> {
    let mut ta = TextArea::default();
    ta.set_cursor_line_style(ratatui::style::Style::default());
    if mask {
        ta.set_mask_char('•');
    }
    ta
}

impl App {
    pub fn new(
        cfg: Config,
        keys: Keymap,
        tg: Tg,
        params: Params,
        images: Images,
        tx: UnboundedSender<AppEvent>,
    ) -> Self {
        let sidebar = true;
        let outbox = outbox::spawn(tg.clone(), tx.clone());
        Self {
            cfg,
            keys,
            tg,
            params,
            store: Store::default(),
            auth: Auth::Starting,
            login_input: login_area(false),
            login_error: None,
            login_busy: false,
            focus: Focus::Tree,
            sidebar,
            tree: TreeState::default(),
            msgs: MessagesState::default(),
            composer: Composer::default(),
            modal: Modal::None,
            current: None,
            images,
            status: None,
            terminal_focused: true,
            rects: Rects::default(),
            last_ranges: Vec::new(),
            wanted: Vec::new(),
            wanted_gaps: Vec::new(),
            tx,
            outbox,
            quit_at: None,
            logging_out: false,
            server_logging_out: false,
            dirty: true,
            connection: None,
            tg_closed: false,
            pending_interrupt: None,
            clipboard: None,
            drafts: HashMap::new(),
            focus_events_seen: false,
            ticks: 0,
        }
    }

    pub fn set_status(&mut self, s: impl Into<String>) {
        self.status = Some((s.into(), Instant::now()));
        self.dirty = true;
    }

    /// Runs until quit. Returns an interrupt when the terminal must be handed
    /// to another program (editor, shell suspend); call again afterwards.
    pub async fn run(
        &mut self,
        terminal: &mut DefaultTerminal,
        td_rx: &mut UnboundedReceiver<Update>,
        app_rx: &mut UnboundedReceiver<AppEvent>,
        signals: &mut Signals,
    ) -> Result<Option<Interrupt>> {
        let mut events = EventStream::new();
        let mut tick = tokio::time::interval(Duration::from_millis(500));
        loop {
            if self.dirty {
                self.dirty = false;
                terminal.draw(|f| crate::ui::draw(f, self))?;
                self.after_draw();
            }
            if let Some(at) = self.quit_at
                && Instant::now() >= at
            {
                return Ok(None);
            }
            tokio::select! {
                ev = events.next() => match ev {
                    Some(Ok(ev)) => self.on_terminal(ev),
                    Some(Err(e)) => tracing::warn!("terminal event: {e}"),
                    None => return Ok(None),
                },
                Some(update) = td_rx.recv() => {
                    self.on_update(update);
                    // Drain bursts (startup sends thousands) before redrawing.
                    while let Ok(u) = td_rx.try_recv() {
                        self.on_update(u);
                    }
                }
                Some(ev) = app_rx.recv() => self.on_app_event(ev),
                _ = signals.term.recv() => self.quit(),
                _ = signals.hup.recv() => self.quit(),
                _ = tick.tick() => {
                    if self.store.expire_typing() {
                        self.dirty = true;
                    }
                    self.ticks += 1;
                    if self.ticks.is_multiple_of(120) {
                        self.evict();
                    }
                    if let Some((_, at)) = &self.status && at.elapsed() > Duration::from_secs(5) {
                        self.status = None;
                        self.dirty = true;
                    }
                }
            }
            if self.quit_at.is_some() && matches!(self.auth, Auth::Closing) && self.tg_closed {
                return Ok(None);
            }
            if let Some(i) = self.pending_interrupt.take() {
                return Ok(Some(i));
            }
        }
    }

    // ---------------------------------------------------------------- TDLib

    fn on_update(&mut self, update: Update) {
        match &update {
            Update::AuthorizationState(s) => {
                self.on_auth(s.authorization_state.clone());
                self.dirty = true;
                return;
            }
            Update::NewMessage(u) => self.maybe_notify(&u.message),
            Update::ConnectionState(s) => {
                let label = match s.state {
                    enums::ConnectionState::WaitingForNetwork => Some("waiting for network…"),
                    enums::ConnectionState::ConnectingToProxy => Some("connecting to proxy…"),
                    enums::ConnectionState::Connecting => Some("connecting…"),
                    enums::ConnectionState::Updating => Some("updating…"),
                    enums::ConnectionState::Ready => None,
                };
                if label.is_none() {
                    // Back online: gaps that failed to load may load now.
                    self.store.retry_failed_gaps(None);
                }
                self.connection = label;
                self.dirty = true;
            }
            Update::MessageSendFailed(u) => {
                self.set_status(format!("send failed: {}", u.error.message));
            }
            Update::ChatFolders(u) => {
                for f in &u.chat_folders {
                    self.spawn_load_chats(ListKey::Folder(f.id));
                }
            }
            _ => {}
        }
        let current_new = matches!(&update, Update::NewMessage(u) if self.shows(&u.message));
        if self.store.apply(&update) {
            self.dirty = true;
        }
        if let Update::NewMessage(u) = &update {
            let chat_id = u.message.chat_id;
            if current_new {
                self.mark_read(chat_id, vec![u.message.id]);
            } else if self.current.is_none_or(|v| v.chat_id != chat_id) {
                self.trim_background(chat_id);
            }
        }
    }

    fn on_auth(&mut self, state: AuthorizationState) {
        // Only the name: the payload holds the phone number and password hint.
        tracing::info!("auth state: {}", auth_state_name(&state));
        self.login_busy = false;
        let prev_qr = matches!(self.auth, Auth::Qr(_));
        self.auth = match state {
            AuthorizationState::WaitTdlibParameters => {
                let tg = self.tg.clone();
                let tx = self.tx.clone();
                let p = self.params.clone();
                tokio::spawn(async move {
                    if let Err(e) = tg.set_parameters(&p).await {
                        let _ = tx.send(AppEvent::AuthError(format!("{e:#}")));
                    }
                });
                Auth::Starting
            }
            AuthorizationState::WaitPhoneNumber => {
                if prev_qr {
                    self.login_error = Some("QR login cancelled or expired".into());
                }
                self.login_input = login_area(false);
                Auth::Phone
            }
            AuthorizationState::WaitOtherDeviceConfirmation(c) => Auth::Qr(c.link),
            AuthorizationState::WaitCode(c) => {
                self.login_input = login_area(false);
                let how = match c.code_info.r#type {
                    enums::AuthenticationCodeType::TelegramMessage(_) => {
                        "sent to your Telegram app"
                    }
                    enums::AuthenticationCodeType::Sms(_) => "sent by SMS",
                    enums::AuthenticationCodeType::Call(_) => "sent by phone call",
                    _ => "sent",
                };
                Auth::Code(format!("Code {how} to {}", c.code_info.phone_number))
            }
            AuthorizationState::WaitPassword(p) => {
                self.login_input = login_area(true);
                Auth::Password(p.password_hint)
            }
            AuthorizationState::WaitEmailAddress(_) => {
                self.login_input = login_area(false);
                Auth::Email
            }
            AuthorizationState::WaitEmailCode(_) => {
                self.login_input = login_area(false);
                Auth::EmailCode
            }
            AuthorizationState::WaitRegistration(_) => Auth::Registration,
            AuthorizationState::WaitPremiumPurchase(_) => {
                self.login_error =
                    Some("Telegram requires Premium to log in this account here.".into());
                Auth::Registration
            }
            AuthorizationState::Ready => {
                self.login_error = None;
                self.on_ready();
                Auth::Ready
            }
            AuthorizationState::LoggingOut => {
                self.server_logging_out = true;
                Auth::Closing
            }
            AuthorizationState::Closing => Auth::Closing,
            AuthorizationState::Closed => {
                self.tg_closed = true;
                // Only wipe the session after Telegram started the logout;
                // a failed or never-sent `logOut` keeps it.
                if self.logging_out && self.server_logging_out {
                    crate::tg::forget_database_key(&self.params.key_store);
                    if let Err(e) = std::fs::remove_dir_all(&self.params.database_dir) {
                        tracing::warn!("removing session: {e}");
                    }
                }
                if self.quit_at.is_none() {
                    self.quit_at = Some(Instant::now());
                }
                Auth::Closing
            }
        };
    }

    fn on_ready(&mut self) {
        let tg = self.tg.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            match functions::get_me(tg.client_id).await {
                Ok(enums::User::User(u)) => {
                    let _ = tx.send(AppEvent::Status(format!(
                        "logged in as {} {}",
                        u.first_name, u.last_name
                    )));
                }
                Err(e) => tracing::warn!("get_me: {}", e.message),
            }
        });
        let tg = self.tg.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            if let Ok(enums::OptionValue::Integer(i)) =
                functions::get_option("my_id".into(), tg.client_id).await
            {
                let _ = tx.send(AppEvent::Me(i.value));
            }
        });
        // TDLib may not send updateScopeNotificationSettings at startup.
        for scope in [
            enums::NotificationSettingsScope::PrivateChats,
            enums::NotificationSettingsScope::GroupChats,
            enums::NotificationSettingsScope::ChannelChats,
        ] {
            let tg = self.tg.clone();
            let tx = self.tx.clone();
            tokio::spawn(async move {
                match functions::get_scope_notification_settings(scope.clone(), tg.client_id).await
                {
                    Ok(enums::ScopeNotificationSettings::ScopeNotificationSettings(s)) => {
                        let _ = tx.send(AppEvent::ScopeMute {
                            scope,
                            mute_for: s.mute_for,
                        });
                    }
                    Err(e) => tracing::warn!("scope notification settings: {}", e.message),
                }
            });
        }
        self.spawn_load_chats(ListKey::Main);
        self.spawn_load_chats(ListKey::Archive);
        self.focus = Focus::Tree;
    }

    fn spawn_load_chats(&self, list: ListKey) {
        let tg = self.tg.clone();
        tokio::spawn(async move { tg.load_all_chats(list.to_list()).await });
    }

    pub(crate) fn on_app_event(&mut self, ev: AppEvent) {
        self.dirty = true;
        match ev {
            AppEvent::History { view, from, result } => {
                self.store.history.entry(view).or_default().loading = false;
                match result {
                    Ok(msgs) => {
                        // Fewer than asked is normal (TDLib returns what it has locally first);
                        // only an empty page means the start of the chat.
                        let got_older = msgs.iter().any(|m| from == 0 || m.id < from);
                        let before = self.store.view_messages(view).len();
                        if view.topic.is_none() {
                            // A chat page is contiguous: it may close a gap.
                            let newer_end = (from != 0).then_some(from);
                            self.store.insert_page(view.chat_id, msgs, newer_end);
                        } else {
                            for m in msgs {
                                self.store.insert_message(m);
                            }
                        }
                        // A page that adds nothing to the view would be asked for again
                        // on every draw.
                        let grew = self.store.view_messages(view).len() > before;
                        if !got_older || (from != 0 && !grew) {
                            self.store.history.entry(view).or_default().reached_start = true;
                        }
                        if Some(view) == self.current && from == 0 {
                            let ids: Vec<i64> = self
                                .store
                                .view_messages(view)
                                .iter()
                                .rev()
                                .take(10)
                                .map(|m| m.id)
                                .collect();
                            self.mark_read(view.chat_id, ids);
                        }
                    }
                    Err(e) => self.set_status(format!("history: {e}")),
                }
            }
            AppEvent::Around {
                view,
                target,
                result,
            } => match result {
                Ok(msgs) => {
                    // Usually far from what is loaded: this records the gap.
                    self.store.insert_page(view.chat_id, msgs, None);
                    if Some(view) == self.current
                        && self.store.message(view.chat_id, target).is_some()
                    {
                        self.msgs.selected = Some(target);
                        self.msgs.reveal = true;
                    }
                }
                Err(e) => self.set_status(format!("jump: {e}")),
            },
            AppEvent::GapPage {
                chat_id,
                newer_end,
                result,
            } => match result {
                Ok(msgs) => {
                    self.store.fill_gap(chat_id, newer_end, msgs);
                    // Messages arrived below the jump target; keep it in view.
                    if self.current.is_some_and(|v| v.chat_id == chat_id)
                        && self.msgs.selected.is_some()
                    {
                        self.msgs.reveal = true;
                    }
                }
                Err(e) => {
                    if let Some(g) = self.store.gap_mut(chat_id, newer_end) {
                        g.loading = false;
                        g.failed = true;
                    }
                    self.set_status(format!("history: {e}"));
                }
            },
            AppEvent::ScopeMute { scope, mute_for } => self.store.scope_mutes.set(&scope, mute_for),
            AppEvent::Topics { chat_id, result } => match result {
                Ok(t) => self.store.set_topics(chat_id, t),
                Err(e) => self.set_status(format!("topics: {e}")),
            },
            AppEvent::File { purpose, result } => match (purpose, result) {
                (FilePurpose::Image(w), Ok(f)) if f.local.is_downloading_completed => {
                    self.spawn_encode(w, f.local.path)
                }
                (FilePurpose::Image(w), Ok(_)) | (FilePurpose::Image(w), Err(_)) => {
                    self.images.entries.insert(w.file_id, image::Entry::Failed);
                }
                (FilePurpose::Open, Ok(f)) => {
                    if can_run_code(std::path::Path::new(&f.local.path)) {
                        self.modal = Modal::ConfirmOpen { path: f.local.path };
                    } else {
                        open_path(&f.local.path);
                    }
                }
                (FilePurpose::Save(dest), Ok(f)) => {
                    // Files can be gigabytes: copy off the event loop.
                    let tx = self.tx.clone();
                    tokio::task::spawn_blocking(move || {
                        let status = match copy_unique(std::path::Path::new(&f.local.path), &dest) {
                            Ok(p) => format!("saved to {}", p.display()),
                            Err(e) => format!("save failed: {e}"),
                        };
                        let _ = tx.send(AppEvent::Status(status));
                    });
                }
                (_, Err(e)) => self.set_status(format!("download failed: {e}")),
            },
            AppEvent::ImageReady { wanted, result } => {
                let (cols, rows) = (wanted.cols, wanted.rows);
                // Only the latest request counts; older sizes finishing late are dropped.
                if self.images.latest_request(wanted.file_id) != Some((cols, rows)) {
                    return;
                }
                let entry = match result {
                    Ok(proto) => image::Entry::Ready {
                        proto,
                        cols,
                        rows,
                        resizing: None,
                    },
                    Err(e) => {
                        tracing::warn!("image {}: {e}", wanted.file_id);
                        image::Entry::Failed
                    }
                };
                self.images.entries.insert(wanted.file_id, entry);
            }
            AppEvent::DeleteRights {
                chat_id,
                message_id,
                result,
            } => {
                let Modal::ConfirmDelete {
                    chat_id: c,
                    message_id: m,
                    rights,
                } = &mut self.modal
                else {
                    return;
                };
                if (*c, *m) != (chat_id, message_id) {
                    return;
                }
                match result {
                    Ok(r) => *rights = Some(r),
                    Err(e) => {
                        self.modal = Modal::None;
                        self.set_status(format!("delete: {e}"));
                    }
                }
            }
            AppEvent::Status(s) => {
                if !s.is_empty() {
                    self.set_status(s);
                }
            }
            AppEvent::Me(id) => self.store.me = Some(id),
            AppEvent::SendFailed { view, draft, error } => {
                // Give the text back: into the composer if it is free, else
                // as that chat's draft for when it is opened again.
                if self.current == Some(view) {
                    if self.composer.is_empty() {
                        self.composer.restore(draft);
                        self.set_status(format!("send failed: {error}"));
                    } else {
                        self.set_status(format!("send failed: {error} (text not restored)"));
                    }
                } else if let std::collections::hash_map::Entry::Vacant(e) = self.drafts.entry(view)
                {
                    e.insert(draft);
                    let chat = self.store.chat_title(view.chat_id);
                    self.set_status(format!(
                        "send failed in {chat}: {error} (text kept as its draft)"
                    ));
                } else {
                    self.set_status(format!("send failed: {error} (text not restored)"));
                }
            }
            AppEvent::LogOutFailed(e) => {
                self.logging_out = false;
                self.set_status(format!("log out failed: {e}"));
            }
            AppEvent::EditReady {
                view,
                message_id,
                original,
                result,
            } => {
                if self.current != Some(view)
                    || self.composer.mode != Mode::Edit(message_id)
                    || !self.composer.is_empty()
                {
                    return;
                }
                match result {
                    Ok(md) => {
                        self.composer.set_text(&md.text);
                        self.composer.edit = Some(EditOrigin {
                            original,
                            initial: md.text.clone(),
                            lossy: composer::formatting_names(&md),
                            warned: false,
                        });
                    }
                    Err(e) => {
                        self.composer.clear();
                        self.set_status(format!("edit: {e}"));
                    }
                }
            }
            AppEvent::AuthError(e) => {
                self.login_busy = false;
                self.login_error = Some(e);
            }
        }
    }

    /// Work that depends on what the last frame showed.
    pub(crate) fn after_draw(&mut self) {
        if let Some(view) = self.current {
            let st = self.store.history.get(&view).copied().unwrap_or_default();
            if self.msgs.at_top && !st.loading && !st.reached_start {
                self.load_history(view);
            }
        }
        if let Some(view) = self.current {
            for newer_end in std::mem::take(&mut self.wanted_gaps) {
                self.fill_gap(view.chat_id, newer_end);
            }
        }
        let wanted = std::mem::take(&mut self.wanted);
        for w in wanted {
            if !self.images.needs(&w) {
                continue;
            }
            self.images.request(&w);
            match self.store.files.get(&w.file_id) {
                Some(f) if f.local.is_downloading_completed => {
                    let path = f.local.path.clone();
                    self.spawn_encode(w, path);
                }
                _ => self.spawn_download(w.file_id, FilePurpose::Image(w), 8),
            }
        }
    }

    fn spawn_encode(&self, w: Wanted, path: String) {
        let Some(picker) = self.images.picker.clone() else {
            return;
        };
        let tx = self.tx.clone();
        tokio::task::spawn_blocking(move || {
            let result =
                image::encode(&picker, &path, w.cols, w.rows).map_err(|e| format!("{e:#}"));
            let _ = tx.send(AppEvent::ImageReady { wanted: w, result });
        });
    }

    fn spawn_download(&self, file_id: i32, purpose: FilePurpose, priority: i32) {
        let tg = self.tg.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = tg
                .download(file_id, priority, true)
                .await
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(AppEvent::File { purpose, result });
        });
    }

    fn load_history(&mut self, view: View) {
        let st = self.store.history.entry(view).or_default();
        if st.loading {
            return;
        }
        st.loading = true;
        let from = self.store.oldest_id(view).unwrap_or(0);
        let limit = self.cfg.messages_limit;
        let tg = self.tg.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = tg
                .history(view, from, limit)
                .await
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(AppEvent::History { view, from, result });
        });
    }

    /// Loads backwards from a gap's newer end, the side the user came from.
    fn fill_gap(&mut self, chat_id: i64, newer_end: i64) {
        let Some(g) = self.store.gap_mut(chat_id, newer_end) else {
            return;
        };
        if g.loading || g.failed {
            return;
        }
        g.loading = true;
        let view = View {
            chat_id,
            topic: None,
        };
        let limit = self.cfg.messages_limit;
        let tg = self.tg.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = tg
                .history(view, newer_end, limit)
                .await
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(AppEvent::GapPage {
                chat_id,
                newer_end,
                result,
            });
        });
    }

    /// Every minute: forget file records and encoded images that no loaded
    /// message uses any more.
    fn evict(&mut self) {
        let live = self.store.evict_files();
        self.images.entries.retain(|id, _| live.contains(id));
    }

    /// Caps the cache of a chat that is not open.
    fn trim_background(&mut self, chat_id: i64) {
        let opened = self.store.history.keys().any(|v| v.chat_id == chat_id);
        let keep = if opened {
            KEEP_BACKGROUND
        } else {
            KEEP_UNOPENED
        };
        self.store.trim_chat(chat_id, keep);
    }

    fn mark_read(&self, chat_id: i64, ids: Vec<i64>) {
        if ids.is_empty() || !self.terminal_focused {
            return;
        }
        self.request(Op::ViewMessages { chat_id, ids });
    }

    fn request(&self, op: Op) {
        if self.outbox.send(op).is_err() {
            tracing::warn!("outbox task is gone");
        }
    }

    /// Whether the open view shows `m`: same chat, and same topic in a forum.
    fn shows(&self, m: &Message) -> bool {
        self.current.is_some_and(|v| {
            v.chat_id == m.chat_id && v.topic.is_none_or(|t| message_in_topic(m, t))
        })
    }

    /// Whether a new message deserves a desktop notification.
    fn would_notify(&self, m: &Message) -> bool {
        let n = &self.cfg.notifications;
        if !n.enabled || m.is_outgoing || !matches!(self.auth, Auth::Ready) {
            return false;
        }
        if !self.store.chats.contains_key(&m.chat_id) || self.store.message_muted(m) {
            return false;
        }
        let viewing = self.shows(m);
        if !self.focus_events_seen {
            // Focus is unknown: notify for everything but the open chat.
            return !viewing;
        }
        if !self.terminal_focused {
            return true;
        }
        // Focused: only with `when_unfocused = false`, and not for the open chat.
        !n.when_unfocused && !viewing
    }

    fn maybe_notify(&self, m: &Message) {
        if !self.would_notify(m) {
            return;
        }
        let Some(chat) = self.store.chats.get(&m.chat_id) else {
            return;
        };
        let title = chat.title.clone();
        let sender = self.store.sender_name(&m.sender_id);
        let body = messages::snippet(m, &self.store);
        let body = if sender == title {
            body
        } else {
            format!("{sender}: {body}")
        };
        crate::notify::send(title, body);
    }

    // ---------------------------------------------------------------- terminal

    fn on_terminal(&mut self, ev: Event) {
        match ev {
            Event::Key(k) if k.kind != KeyEventKind::Release => {
                self.dirty = true;
                self.on_key(k);
            }
            Event::Mouse(m) if self.cfg.mouse => self.on_mouse(m),
            Event::Paste(s) => {
                if self.focus == Focus::Composer && matches!(self.auth, Auth::Ready) {
                    self.composer.ta.insert_str(&s);
                } else if !matches!(self.auth, Auth::Ready) {
                    self.login_input.insert_str(s.trim());
                }
                self.dirty = true;
            }
            Event::FocusGained => {
                self.focus_events_seen = true;
                self.terminal_focused = true;
                if let Some(v) = self.current {
                    let ids: Vec<i64> = self
                        .store
                        .view_messages(v)
                        .iter()
                        .rev()
                        .take(10)
                        .map(|m| m.id)
                        .collect();
                    self.mark_read(v.chat_id, ids);
                }
            }
            Event::FocusLost => {
                self.focus_events_seen = true;
                self.terminal_focused = false;
            }
            Event::Resize(..) => self.dirty = true,
            _ => {}
        }
    }

    pub(crate) fn on_key(&mut self, k: KeyEvent) {
        // Quit works everywhere, even mid-login.
        if self.keys.global.lookup(&k) == Some(GlobalAction::Quit) {
            self.quit();
            return;
        }
        if !matches!(self.auth, Auth::Ready) {
            self.on_login_key(k);
            return;
        }
        match &mut self.modal {
            Modal::Picker(_) => return self.on_picker_key(k),
            Modal::Help { scroll } => {
                use crossterm::event::KeyCode;
                match k.code {
                    KeyCode::Char('j') | KeyCode::Down => *scroll = scroll.saturating_add(1),
                    KeyCode::Char('k') | KeyCode::Up => *scroll = scroll.saturating_sub(1),
                    KeyCode::PageDown => *scroll = scroll.saturating_add(10),
                    KeyCode::PageUp => *scroll = scroll.saturating_sub(10),
                    _ => self.modal = Modal::None,
                }
                return;
            }
            Modal::ConfirmDelete {
                chat_id,
                message_id,
                rights,
            } => {
                use crossterm::event::KeyCode;
                let (chat_id, message_id, rights) = (*chat_id, *message_id, *rights);
                let yes = matches!(k.code, KeyCode::Char('y' | 'Y'));
                let me_only = matches!(k.code, KeyCode::Char('m' | 'M'));
                let revoke = match rights {
                    // Still asking TDLib what is allowed: only y/m wait for it.
                    None if yes || me_only => return,
                    None => None,
                    Some(r) if yes && r.for_everyone => Some(true),
                    Some(r) if yes && r.only_for_self => Some(false),
                    Some(r) if me_only && r.for_everyone && r.only_for_self => Some(false),
                    Some(_) => None,
                };
                self.modal = Modal::None;
                if let Some(revoke) = revoke {
                    self.request(Op::Delete {
                        chat_id,
                        message_id,
                        revoke,
                    });
                }
                return;
            }
            Modal::ConfirmOpen { path } => {
                let path = std::mem::take(path);
                self.modal = Modal::None;
                if matches!(k.code, crossterm::event::KeyCode::Char('y' | 'Y')) {
                    open_path(&path);
                }
                return;
            }
            Modal::ConfirmLogout => {
                self.modal = Modal::None;
                if matches!(k.code, crossterm::event::KeyCode::Char('y' | 'Y')) {
                    self.logging_out = true;
                    self.request(Op::LogOut);
                    self.set_status("logging out…");
                }
                return;
            }
            Modal::None => {}
        }
        if let Some(a) = self.keys.global.lookup(&k) {
            return self.on_global(a);
        }
        match self.focus {
            Focus::Tree => {
                if let Some(a) = self.keys.tree.lookup(&k) {
                    self.on_tree(a);
                }
            }
            Focus::Messages => {
                if let Some(a) = self.keys.messages.lookup(&k) {
                    self.on_messages(a);
                }
            }
            Focus::Composer => self.on_composer_key(k),
        }
    }

    fn quit(&mut self) {
        if self.quit_at.is_some() {
            return;
        }
        self.quit_at = Some(Instant::now() + Duration::from_secs(3));
        // Through the outbox, so sends queued before it still reach TDLib.
        self.request(Op::Close);
        if !matches!(self.auth, Auth::Ready) && !matches!(self.auth, Auth::Starting) {
            // Nothing to flush mid-login; leave right away.
            self.quit_at = Some(Instant::now());
        }
    }

    fn on_global(&mut self, a: GlobalAction) {
        match a {
            GlobalAction::FocusChats => {
                self.sidebar = true;
                self.focus = Focus::Tree;
            }
            GlobalAction::FocusMessages => self.focus = Focus::Messages,
            GlobalAction::FocusComposer => {
                if self.current.is_some() {
                    self.focus = Focus::Composer;
                }
            }
            GlobalAction::FocusNext | GlobalAction::FocusPrevious => {
                let mut order = vec![];
                if self.sidebar {
                    order.push(Focus::Tree);
                }
                order.push(Focus::Messages);
                if self.current.is_some() {
                    order.push(Focus::Composer);
                }
                let i = order.iter().position(|f| *f == self.focus).unwrap_or(0);
                let n = order.len();
                let j = if a == GlobalAction::FocusNext {
                    (i + 1) % n
                } else {
                    (i + n - 1) % n
                };
                self.focus = order[j];
            }
            GlobalAction::ToggleSidebar => {
                self.sidebar = !self.sidebar;
                if !self.sidebar && self.focus == Focus::Tree {
                    self.focus = Focus::Messages;
                }
            }
            GlobalAction::Picker => self.modal = Modal::Picker(ChatPicker::new(&self.store)),
            GlobalAction::ToggleHelp => self.modal = Modal::Help { scroll: 0 },
            GlobalAction::Suspend => self.pending_interrupt = Some(Interrupt::Suspend),
            GlobalAction::Logout => {
                if !self.logging_out {
                    self.modal = Modal::ConfirmLogout;
                }
            }
            GlobalAction::Quit => self.quit(),
        }
    }

    // ---------------------------------------------------------------- tree

    fn on_tree(&mut self, a: TreeAction) {
        let rows = chats_tree::rows(&self.store, &self.tree);
        if rows.is_empty() {
            return;
        }
        let i = chats_tree::index_of(&rows, self.tree.selected);
        let page = self.rects.tree_inner.height.max(1) as usize;
        let set = |s: &mut Self, j: usize| s.tree.selected = Some(rows[j.min(rows.len() - 1)].id);
        match a {
            TreeAction::Up => set(self, i.saturating_sub(1)),
            TreeAction::Down => set(self, i + 1),
            TreeAction::Top => set(self, 0),
            TreeAction::Bottom => set(self, rows.len() - 1),
            TreeAction::PageUp => set(self, i.saturating_sub(page)),
            TreeAction::PageDown => set(self, i + page),
            TreeAction::Select => {
                let row = rows[i].clone();
                self.tree.selected = Some(row.id);
                self.activate_node(row.id, row.expandable);
            }
            TreeAction::CollapseParent => {
                let id = rows[i].id;
                if rows[i].expanded {
                    self.tree.expanded.remove(&id);
                } else if let Some(p) = id.parent() {
                    self.tree.expanded.remove(&p);
                    self.tree.selected = Some(p);
                }
            }
            TreeAction::CollapseAll => {
                self.tree.expanded.clear();
                // Select the top-level list the row was in.
                let mut top = rows[i].id;
                while let Some(p) = top.parent() {
                    top = p;
                }
                self.tree.selected = Some(top);
            }
            TreeAction::MoveToParent => {
                if let Some(p) = rows[i].id.parent() {
                    self.tree.selected = Some(p);
                }
            }
            TreeAction::NextUnread | TreeAction::PrevUnread => {
                if let Some(j) = chats_tree::next_unread(&rows, i, a == TreeAction::NextUnread) {
                    set(self, j);
                } else {
                    self.set_status("no unread chats");
                }
            }
            TreeAction::YankId => {
                let id = match rows[i].id {
                    NodeId::List(_) => return,
                    NodeId::Chat(_, c) => c.to_string(),
                    NodeId::Topic(_, c, t) => format!("{c}/{t}"),
                };
                self.yank(id);
            }
        }
    }

    fn activate_node(&mut self, id: NodeId, expandable: bool) {
        match id {
            NodeId::List(_) => toggle(&mut self.tree.expanded, id),
            NodeId::Chat(_, chat_id) if expandable => {
                toggle(&mut self.tree.expanded, id);
                if self.tree.expanded.contains(&id) {
                    self.load_topics(chat_id);
                }
            }
            _ => {
                if let Some(v) = id.view() {
                    self.open_view(v);
                }
            }
        }
    }

    fn load_topics(&self, chat_id: i64) {
        let tg = self.tg.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = tg.forum_topics(chat_id).await.map_err(|e| format!("{e:#}"));
            let _ = tx.send(AppEvent::Topics { chat_id, result });
        });
    }

    pub fn open_view(&mut self, view: View) {
        if self.current == Some(view) {
            self.focus = Focus::Composer;
            return;
        }
        let prev = self.current.replace(view);
        if let Some(p) = prev {
            // Keep the unsent text of the chat being left.
            if self.composer.is_empty() {
                self.drafts.remove(&p);
            } else {
                self.drafts.insert(p, self.composer.draft());
            }
            if p.chat_id != view.chat_id {
                self.request(Op::CloseChat(p.chat_id));
                self.trim_background(p.chat_id);
            }
        }
        self.request(Op::OpenChat(view.chat_id));
        self.store.retry_failed_gaps(Some(view.chat_id));
        self.msgs = MessagesState::default();
        self.composer.clear();
        if let Some(d) = self.drafts.remove(&view) {
            self.composer.restore(d);
        }
        self.load_history(view);
        if self.store.is_forum(view.chat_id) && view.topic.is_none() {
            self.load_topics(view.chat_id);
        }
        self.focus = Focus::Composer;
    }

    // ---------------------------------------------------------------- messages

    fn selected_message(&self) -> Option<&Message> {
        let v = self.current?;
        self.store.message(v.chat_id, self.msgs.selected?)
    }

    fn on_messages(&mut self, a: MessagesAction) {
        let Some(view) = self.current else {
            return;
        };
        let ids: Vec<i64> = self
            .store
            .view_messages(view)
            .iter()
            .map(|m| m.id)
            .collect();
        let pos = self
            .msgs
            .selected
            .and_then(|s| ids.iter().position(|&i| i == s));
        let page = self.rects.messages_inner.height.max(1) as usize;
        let select = |s: &mut Self, i: usize| {
            if let Some(&id) = ids.get(i) {
                s.msgs.selected = Some(id);
                s.msgs.reveal = true;
            }
        };
        match a {
            MessagesAction::Up => match pos {
                Some(p) => select(self, p.saturating_sub(1)),
                None if !ids.is_empty() => select(self, ids.len() - 1),
                None => {}
            },
            MessagesAction::Down => match pos {
                Some(p) if p + 1 < ids.len() => select(self, p + 1),
                Some(_) => {
                    self.msgs.selected = None;
                    self.msgs.scroll = 0;
                }
                None => {}
            },
            MessagesAction::Top => select(self, 0),
            MessagesAction::Bottom => {
                self.msgs.selected = None;
                self.msgs.scroll = 0;
            }
            MessagesAction::ScrollUp => self.msgs.scroll += page / 2,
            MessagesAction::ScrollDown => {
                self.msgs.scroll = self.msgs.scroll.saturating_sub(page / 2)
            }
            MessagesAction::Cancel => self.msgs.selected = None,
            MessagesAction::SelectReply => {
                let Some(m) = self.selected_message() else {
                    return;
                };
                let Some(enums::MessageReplyTo::Message(r)) = &m.reply_to else {
                    return self.set_status("not a reply");
                };
                if r.chat_id != 0 && r.chat_id != view.chat_id {
                    return self.set_status("reply is in another chat");
                }
                let target = r.message_id;
                if self.store.message(view.chat_id, target).is_some() {
                    self.msgs.selected = Some(target);
                    self.msgs.reveal = true;
                } else {
                    let tg = self.tg.clone();
                    let tx = self.tx.clone();
                    tokio::spawn(async move {
                        let result = tg
                            .history_around(view.chat_id, target, 20)
                            .await
                            .map_err(|e| format!("{e:#}"));
                        let _ = tx.send(AppEvent::Around {
                            view,
                            target,
                            result,
                        });
                    });
                }
            }
            MessagesAction::Reply => {
                if let Some(id) = self.msgs.selected {
                    self.composer.mode = Mode::Reply(id);
                    self.focus = Focus::Composer;
                }
            }
            MessagesAction::Edit => {
                if let Some(m) = self.selected_message() {
                    self.start_edit(m.id);
                }
            }
            MessagesAction::Delete => {
                if let Some(m) = self.selected_message() {
                    let (chat_id, message_id) = (m.chat_id, m.id);
                    self.modal = Modal::ConfirmDelete {
                        chat_id,
                        message_id,
                        rights: None,
                    };
                    let tg = self.tg.clone();
                    let tx = self.tx.clone();
                    tokio::spawn(async move {
                        let result = tg
                            .delete_rights(chat_id, message_id)
                            .await
                            .map(|(for_everyone, only_for_self)| DeleteRights {
                                for_everyone,
                                only_for_self,
                            })
                            .map_err(|e| format!("{e:#}"));
                        let _ = tx.send(AppEvent::DeleteRights {
                            chat_id,
                            message_id,
                            result,
                        });
                    });
                }
            }
            MessagesAction::Open => self.attachment(None),
            MessagesAction::Download => {
                let dir = directories::UserDirs::new()
                    .and_then(|d| d.download_dir().map(|p| p.to_path_buf()))
                    .unwrap_or_else(std::env::temp_dir);
                self.attachment(Some(dir));
            }
            MessagesAction::YankText => {
                if let Some(m) = self.selected_message() {
                    let t = messages::parts(m, &self.store)
                        .1
                        .map(|t| t.text)
                        .unwrap_or_default();
                    self.yank(t);
                }
            }
            MessagesAction::YankUrl => {
                if let Some(m) = self.selected_message() {
                    let urls = messages::parts(m, &self.store)
                        .1
                        .map(|t| crate::ui::render_text::urls(&t))
                        .unwrap_or_default();
                    match urls.into_iter().next() {
                        Some(u) => self.yank(u),
                        None => self.set_status("no link in message"),
                    }
                }
            }
            MessagesAction::YankId => {
                if let Some(id) = self.msgs.selected {
                    self.yank(id.to_string());
                }
            }
        }
    }

    fn start_edit(&mut self, id: i64) {
        let Some(view) = self.current else { return };
        let Some(m) = self.store.message(view.chat_id, id) else {
            return;
        };
        if !m.is_outgoing {
            return self.set_status("can only edit your own messages");
        }
        let original = match &m.content {
            MessageContent::MessageText(t) => t.text.clone(),
            _ => return self.set_status("only text messages can be edited"),
        };
        self.composer.clear();
        self.composer.mode = Mode::Edit(id);
        self.focus = Focus::Composer;
        let formatting = composer::formatting_names(&original);
        if formatting.is_empty() || !self.cfg.markdown.enabled {
            // Plain text is edited as is: markdown parsing could turn
            // `__init__` into italics.
            self.composer.set_text(&original.text);
            self.composer.edit = Some(EditOrigin {
                initial: original.text.clone(),
                original,
                lossy: formatting,
                warned: false,
            });
            return;
        }
        // Formatted text: TDLib writes the markdown; it fills the composer
        // when it answers (a local call, so right away).
        let tg = self.tg.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = tg
                .markdown_of(original.clone())
                .await
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(AppEvent::EditReady {
                view,
                message_id: id,
                original,
                result,
            });
        });
    }

    /// Open (`dest` None) or save the selected message's file.
    fn attachment(&mut self, dest: Option<PathBuf>) {
        let Some(m) = self.selected_message() else {
            return;
        };
        let file = match &m.content {
            MessageContent::MessagePhoto(p) => p
                .photo
                .sizes
                .iter()
                .max_by_key(|s| s.width)
                .map(|s| (s.photo.clone(), format!("photo_{}.jpg", m.id))),
            MessageContent::MessageDocument(d) => Some((
                d.document.document.clone(),
                non_empty(&d.document.file_name, format!("file_{}", m.id)),
            )),
            MessageContent::MessageVideo(v) => Some((
                v.video.video.clone(),
                non_empty(&v.video.file_name, format!("video_{}.mp4", m.id)),
            )),
            MessageContent::MessageAnimation(a) => Some((
                a.animation.animation.clone(),
                non_empty(&a.animation.file_name, format!("gif_{}.mp4", m.id)),
            )),
            MessageContent::MessageAudio(a) => Some((
                a.audio.audio.clone(),
                non_empty(&a.audio.file_name, format!("audio_{}.mp3", m.id)),
            )),
            MessageContent::MessageVoiceNote(v) => {
                Some((v.voice_note.voice.clone(), format!("voice_{}.ogg", m.id)))
            }
            MessageContent::MessageVideoNote(v) => Some((
                v.video_note.video.clone(),
                format!("video_note_{}.mp4", m.id),
            )),
            MessageContent::MessageSticker(s) => {
                Some((s.sticker.sticker.clone(), format!("sticker_{}.webp", m.id)))
            }
            _ => None,
        };
        let fallback = format!("file_{}", m.id);
        let Some((file, name)) = file else {
            return self.set_status("no attachment");
        };
        let purpose = match dest {
            None => FilePurpose::Open,
            Some(dir) => FilePurpose::Save(dir.join(sanitize(&name, &fallback))),
        };
        self.set_status("downloading…");
        self.spawn_download(file.id, purpose, 32);
    }

    fn yank(&mut self, text: String) {
        if self.clipboard.is_none() {
            match arboard::Clipboard::new() {
                Ok(c) => self.clipboard = Some(c),
                Err(e) => tracing::debug!("clipboard: {e}"),
            }
        }
        let result = match &mut self.clipboard {
            Some(c) => c.set_text(text.clone()).map_err(|e| e.to_string()),
            None => Err("no system clipboard".into()),
        };
        match result {
            Ok(()) => self.set_status("copied"),
            Err(e) => {
                // Over SSH or without a display: ask the terminal to copy.
                self.clipboard = None;
                tracing::debug!("clipboard: {e}; using OSC 52");
                use std::io::Write;
                let mut out = std::io::stdout();
                let _ = out.write_all(osc52(&text).as_bytes());
                let _ = out.flush();
                self.set_status("copied through the terminal (OSC 52)");
            }
        }
    }

    // ---------------------------------------------------------------- composer

    fn on_composer_key(&mut self, k: KeyEvent) {
        match self.keys.composer.lookup(&k) {
            Some(ComposerAction::Send) => return self.send(),
            Some(ComposerAction::Newline) => {
                self.composer.ta.insert_newline();
                return;
            }
            Some(ComposerAction::Cancel) => {
                if self.composer.mode != Mode::Normal {
                    let was_edit = matches!(self.composer.mode, Mode::Edit(_));
                    self.composer.mode = Mode::Normal;
                    if was_edit {
                        self.composer.clear();
                    }
                } else if !self.composer.is_empty() {
                    self.composer.clear();
                } else {
                    self.focus = Focus::Messages;
                }
                return;
            }
            Some(ComposerAction::EditLast) if self.composer.is_empty() => {
                if let Some(view) = self.current {
                    let last = self
                        .store
                        .view_messages(view)
                        .iter()
                        .rev()
                        // A message still being sent has a temporary id.
                        .find(|m| m.is_outgoing && m.sending_state.is_none())
                        .map(|m| m.id);
                    if let Some(id) = last {
                        self.start_edit(id);
                    }
                }
                return;
            }
            Some(ComposerAction::OpenEditor) => {
                self.pending_interrupt = Some(Interrupt::Editor);
                return;
            }
            _ => {}
        }
        if self.composer.ta.input(k) {
            self.send_typing();
        }
    }

    fn send_typing(&mut self) {
        if !self.cfg.typing_indicator.send || !self.composer.should_send_typing() {
            return;
        }
        let Some(view) = self.current else { return };
        let tg = self.tg.clone();
        tokio::spawn(async move {
            let _ = functions::send_chat_action(
                view.chat_id,
                view.message_topic(),
                Some(enums::ChatAction::Typing),
                tg.client_id,
            )
            .await;
        });
    }

    fn send(&mut self) {
        let Some(view) = self.current else { return };
        let text = self.composer.text();
        if text.trim().is_empty() {
            return;
        }
        let markdown = self.cfg.markdown.enabled;
        let typed = Body::Typed {
            text: text.clone(),
            markdown,
        };
        let op = match self.composer.mode {
            Mode::Edit(message_id) => {
                let body = match self.composer.edit.as_mut() {
                    Some(origin) if text == origin.initial => {
                        self.composer.clear();
                        return self.set_status("no changes");
                    }
                    Some(origin) if !origin.lossy.is_empty() && !origin.warned => {
                        origin.warned = true;
                        let what = origin.lossy.join(", ");
                        let key = self.keys.composer.label(ComposerAction::Send);
                        return self.set_status(format!(
                            "saving this edit removes its {what}; {key} again to save"
                        ));
                    }
                    // A message without formatting stays plain: no markdown parsing.
                    Some(origin) if composer::formatting_names(&origin.original).is_empty() => {
                        Body::Exact(tdlib_rs::types::FormattedText {
                            text,
                            entities: vec![],
                        })
                    }
                    _ => typed,
                };
                Op::Edit {
                    view,
                    message_id,
                    body,
                    draft: self.composer.draft(),
                }
            }
            Mode::Reply(id) => Op::Send {
                view,
                reply_to: Some(id),
                body: typed,
                draft: self.composer.draft(),
            },
            Mode::Normal => Op::Send {
                view,
                reply_to: None,
                body: typed,
                draft: self.composer.draft(),
            },
        };
        self.request(op);
        self.composer.clear();
        self.composer.last_typing_sent = None;
        self.msgs.selected = None;
        self.msgs.scroll = 0;
    }

    /// Text currently in the composer, for `$EDITOR`.
    pub fn composer_text(&self) -> String {
        self.composer.text()
    }

    pub fn set_composer_text(&mut self, text: &str) {
        let mode = self.composer.mode;
        self.composer.set_text(text.trim_end_matches('\n'));
        self.composer.mode = mode;
        self.focus = Focus::Composer;
        self.dirty = true;
    }

    // ---------------------------------------------------------------- picker

    fn on_picker_key(&mut self, k: KeyEvent) {
        let Modal::Picker(p) = &mut self.modal else {
            return;
        };
        match self.keys.picker.lookup(&k) {
            Some(PickerAction::Up) => p.move_by(-1),
            Some(PickerAction::Down) => p.move_by(1),
            Some(PickerAction::Cancel) => self.modal = Modal::None,
            Some(PickerAction::Select) => {
                let v = p.current();
                self.modal = Modal::None;
                if let Some(v) = v {
                    self.reveal_in_tree(v);
                    self.open_view(v);
                }
            }
            None => {
                use crossterm::event::KeyCode;
                match k.code {
                    KeyCode::Backspace => {
                        p.query.pop();
                        p.refilter();
                    }
                    KeyCode::Char(c)
                        if !k.modifiers.intersects(
                            crossterm::event::KeyModifiers::CONTROL
                                | crossterm::event::KeyModifiers::ALT,
                        ) =>
                    {
                        p.query.push(c);
                        p.refilter();
                    }
                    _ => {}
                }
            }
        }
    }

    fn reveal_in_tree(&mut self, v: View) {
        // Prefer All chats, then Archive, then any folder.
        let list = self
            .store
            .chats
            .get(&v.chat_id)
            .and_then(|c| {
                let lists: Vec<ListKey> = c
                    .positions
                    .iter()
                    .map(|p| ListKey::from_list(&p.list))
                    .collect();
                [ListKey::Main, ListKey::Archive]
                    .into_iter()
                    .find(|l| lists.contains(l))
                    .or_else(|| lists.first().copied())
            })
            .unwrap_or(ListKey::Main);
        self.tree.expanded.insert(NodeId::List(list));
        let node = match v.topic {
            Some(t) => {
                self.tree.expanded.insert(NodeId::Chat(list, v.chat_id));
                NodeId::Topic(list, v.chat_id, t)
            }
            None => NodeId::Chat(list, v.chat_id),
        };
        self.tree.selected = Some(node);
    }

    // ---------------------------------------------------------------- login

    fn on_login_key(&mut self, k: KeyEvent) {
        use crossterm::event::KeyCode;
        if self.login_busy {
            return;
        }
        match (&self.auth, k.code) {
            (Auth::Phone, KeyCode::Tab) => {
                self.login_busy = true;
                let tg = self.tg.clone();
                let tx = self.tx.clone();
                tokio::spawn(async move {
                    if let Err(e) =
                        functions::request_qr_code_authentication(vec![], tg.client_id).await
                    {
                        let _ = tx.send(AppEvent::AuthError(e.message));
                    }
                });
            }
            (
                Auth::Phone | Auth::Code(_) | Auth::Password(_) | Auth::Email | Auth::EmailCode,
                KeyCode::Enter,
            ) => {
                let value = self.login_input.lines().join("").trim().to_string();
                if value.is_empty() {
                    return;
                }
                self.login_busy = true;
                self.login_error = None;
                let auth = self.auth.clone();
                let tg = self.tg.clone();
                let tx = self.tx.clone();
                tokio::spawn(async move {
                    let id = tg.client_id;
                    let res = match auth {
                        Auth::Phone => {
                            functions::set_authentication_phone_number(value, None, id).await
                        }
                        Auth::Code(_) => functions::check_authentication_code(value, id).await,
                        Auth::Password(_) => {
                            functions::check_authentication_password(value, id).await
                        }
                        Auth::Email => functions::set_authentication_email_address(value, id).await,
                        Auth::EmailCode => {
                            functions::check_authentication_email_code(
                                enums::EmailAddressAuthentication::Code(
                                    tdlib_rs::types::EmailAddressAuthenticationCode { code: value },
                                ),
                                id,
                            )
                            .await
                        }
                        _ => Ok(()),
                    };
                    // Success arrives as a new authorization state.
                    if let Err(e) = res {
                        let _ = tx.send(AppEvent::AuthError(e.message));
                    }
                });
            }
            (
                Auth::Qr(_) | Auth::Starting | Auth::Registration | Auth::Closing | Auth::Ready,
                _,
            ) => {}
            _ => {
                self.login_input.input(k);
            }
        }
    }

    // ---------------------------------------------------------------- mouse

    fn on_mouse(&mut self, m: crossterm::event::MouseEvent) {
        if !matches!(self.auth, Auth::Ready) || !matches!(self.modal, Modal::None) {
            return;
        }
        let pos = ratatui::layout::Position {
            x: m.column,
            y: m.row,
        };
        let r = self.rects;
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.dirty = true;
                if self.sidebar && r.tree.contains(pos) {
                    self.focus = Focus::Tree;
                    if r.tree_inner.contains(pos) {
                        let rows = chats_tree::rows(&self.store, &self.tree);
                        let idx = self.tree.offset + (m.row - r.tree_inner.y) as usize;
                        if let Some(row) = rows.get(idx).cloned() {
                            self.tree.selected = Some(row.id);
                            self.activate_node(row.id, row.expandable);
                        }
                    }
                } else if r.messages.contains(pos) {
                    self.focus = Focus::Messages;
                    if r.messages_inner.contains(pos)
                        && m.row >= r.messages_inner.y + r.messages_pad
                    {
                        let line =
                            r.messages_top + (m.row - r.messages_inner.y - r.messages_pad) as usize;
                        if let Some(&(id, _, _)) = self
                            .last_ranges
                            .iter()
                            .find(|(_, a, b)| line >= *a && line < *b)
                        {
                            self.msgs.selected = Some(id);
                        }
                    }
                } else if r.composer.contains(pos) && self.current.is_some() {
                    self.focus = Focus::Composer;
                }
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                self.dirty = true;
                let up = matches!(m.kind, MouseEventKind::ScrollUp);
                if r.messages.contains(pos) {
                    if up {
                        self.msgs.scroll += 3;
                    } else {
                        self.msgs.scroll = self.msgs.scroll.saturating_sub(3);
                    }
                } else if self.sidebar && r.tree.contains(pos) {
                    self.on_tree(if up { TreeAction::Up } else { TreeAction::Down });
                }
            }
            _ => {}
        }
    }
}

fn toggle(set: &mut std::collections::HashSet<NodeId>, id: NodeId) {
    if !set.remove(&id) {
        set.insert(id);
    }
}

fn non_empty(s: &str, fallback: String) -> String {
    if s.is_empty() {
        fallback
    } else {
        s.to_string()
    }
}

/// `dir/name.ext`, then `dir/name (1).ext`, `dir/name (2).ext` and so on.
fn path_candidates(p: &std::path::Path) -> impl Iterator<Item = PathBuf> + '_ {
    let stem = p
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let ext = p
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    let dir = p.parent().map(|d| d.to_path_buf()).unwrap_or_default();
    std::iter::once(p.to_path_buf())
        .chain((1..).map(move |n| dir.join(format!("{stem} ({n}){ext}"))))
}

/// The first candidate of `p` that does not exist yet.
#[cfg(test)]
fn unique_path(p: &std::path::Path) -> PathBuf {
    path_candidates(p)
        .find(|c| !c.exists())
        .expect("some free name")
}

/// Copies `src` to the first free name for `dest`, never overwriting a file
/// (two saves of the same name at once each get their own). Returns the path.
fn copy_unique(src: &std::path::Path, dest: &std::path::Path) -> std::io::Result<PathBuf> {
    let mut input = std::fs::File::open(src)?;
    for c in path_candidates(dest) {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&c)
        {
            Ok(mut out) => {
                if let Err(e) = std::io::copy(&mut input, &mut out) {
                    let _ = std::fs::remove_file(&c);
                    return Err(e);
                }
                return Ok(c);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    unreachable!("candidates never run out")
}

/// OSC 52 "set clipboard" sequence, which most terminals (and tmux with
/// `set-clipboard on`) turn into a system copy.
fn osc52(text: &str) -> String {
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD.encode(text);
    format!("\x1b]52;c;{b64}\x07")
}

/// A file name that stays inside its directory: no separators or control
/// characters, and never empty, `.` or `..` (those become `fallback`).
fn sanitize(name: &str, fallback: &str) -> String {
    let clean: String = name
        .chars()
        .filter(|c| !c.is_control())
        .map(|c| if c == '/' || c == '\\' { '_' } else { c })
        .collect();
    match clean.trim() {
        "" | "." | ".." => fallback.to_string(),
        _ => clean,
    }
}

/// Name of an authorization state, without its payload.
fn auth_state_name(s: &AuthorizationState) -> &'static str {
    match s {
        AuthorizationState::WaitTdlibParameters => "wait_tdlib_parameters",
        AuthorizationState::WaitPhoneNumber => "wait_phone_number",
        AuthorizationState::WaitOtherDeviceConfirmation(_) => "wait_other_device_confirmation",
        AuthorizationState::WaitCode(_) => "wait_code",
        AuthorizationState::WaitPassword(_) => "wait_password",
        AuthorizationState::WaitEmailAddress(_) => "wait_email_address",
        AuthorizationState::WaitEmailCode(_) => "wait_email_code",
        AuthorizationState::WaitRegistration(_) => "wait_registration",
        AuthorizationState::WaitPremiumPurchase(_) => "wait_premium_purchase",
        AuthorizationState::Ready => "ready",
        AuthorizationState::LoggingOut => "logging_out",
        AuthorizationState::Closing => "closing",
        AuthorizationState::Closed => "closed",
    }
}

/// Extensions of files that can run code when opened, modelled on Telegram
/// Desktop's list of executable extensions, plus macOS and Linux launchers.
const RISKY_EXTENSIONS: &[&str] = &[
    // Windows
    "ad",
    "ade",
    "adp",
    "app",
    "application",
    "appref-ms",
    "asp",
    "asx",
    "bas",
    "bat",
    "bin",
    "cer",
    "chm",
    "cmd",
    "cnt",
    "com",
    "cpl",
    "crt",
    "csh",
    "der",
    "diagcab",
    "dll",
    "drv",
    "exe",
    "fxp",
    "gadget",
    "grp",
    "hlp",
    "hpj",
    "hta",
    "htt",
    "inf",
    "ini",
    "ins",
    "inx",
    "isp",
    "isu",
    "its",
    "jar",
    "jnlp",
    "job",
    "js",
    "jse",
    "ksh",
    "lnk",
    "mad",
    "maf",
    "mag",
    "mam",
    "maq",
    "mar",
    "mas",
    "mat",
    "mau",
    "mav",
    "maw",
    "mcf",
    "mda",
    "mdb",
    "mde",
    "mdt",
    "mdw",
    "mdz",
    "mht",
    "mhtml",
    "mjs",
    "mmc",
    "mof",
    "msc",
    "msh",
    "msh1",
    "msh2",
    "mshxml",
    "msi",
    "msp",
    "mst",
    "ops",
    "osd",
    "pcd",
    "pif",
    "plg",
    "prf",
    "prg",
    "ps1",
    "ps1xml",
    "ps2",
    "ps2xml",
    "psc1",
    "psc2",
    "psd1",
    "psm1",
    "pst",
    "reg",
    "rgs",
    "scf",
    "scr",
    "sct",
    "search-ms",
    "settingcontent-ms",
    "shb",
    "shs",
    "slk",
    "sys",
    "u3p",
    "url",
    "vb",
    "vbe",
    "vbp",
    "vbs",
    "vbscript",
    "vsmacros",
    "vsw",
    "ws",
    "wsc",
    "wsf",
    "wsh",
    "xbap",
    "xll",
    "xnk",
    // Scripts
    "sh",
    "bash",
    "zsh",
    "fish",
    "py",
    "pyc",
    "pyo",
    "pyw",
    "pyz",
    "pl",
    "pm",
    "rb",
    "php",
    "phar",
    "lua",
    "tcl",
    "ps",
    "applescript",
    "scpt",
    "scptd",
    // macOS
    "action",
    "command",
    "dmg",
    "fileloc",
    "inetloc",
    "kext",
    "mpkg",
    "pkg",
    "prefpane",
    "saver",
    "terminal",
    "tool",
    "webloc",
    "workflow",
    // Linux
    "appimage",
    "deb",
    "desktop",
    "flatpakref",
    "rpm",
    "run",
    "snap",
    // Pages a browser runs with access to local files
    "htm",
    "html",
    "shtml",
    "svg",
    "xht",
    "xhtml",
];

/// Whether opening `path` with the system handler can run code: a risky
/// extension, or the executable bit.
fn can_run_code(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let risky_ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .is_some_and(|e| RISKY_EXTENSIONS.contains(&e.as_str()));
    let executable = std::fs::metadata(path).is_ok_and(|m| m.permissions().mode() & 0o111 != 0);
    risky_ext || executable
}

#[cfg(test)]
thread_local! {
    /// Paths `open_path` would have opened.
    static OPENED: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
}

fn open_path(path: &str) {
    #[cfg(test)]
    {
        OPENED.with(|o| o.borrow_mut().push(path.to_string()));
    }
    #[cfg(not(test))]
    {
        quarantine(path);
        let opener = if cfg!(target_os = "macos") {
            "open"
        } else {
            "xdg-open"
        };
        match std::process::Command::new(opener)
            .arg(path)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            // Reap the child so it does not linger as a zombie.
            Ok(mut child) => {
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
            }
            Err(e) => tracing::warn!("{opener}: {e}"),
        }
    }
}

/// Marks a downloaded file the way browsers do, so Gatekeeper checks it on
/// first open. Keeps an existing mark.
#[cfg(all(target_os = "macos", not(test)))]
fn quarantine(path: &str) {
    let Ok(cpath) = std::ffi::CString::new(path) else {
        return;
    };
    let name = c"com.apple.quarantine";
    // SAFETY: valid C strings; a null buffer only asks for the size.
    unsafe {
        if libc::getxattr(cpath.as_ptr(), name.as_ptr(), std::ptr::null_mut(), 0, 0, 0) >= 0 {
            return;
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let value = format!("0081;{now:08x};telegordo;");
        if libc::setxattr(
            cpath.as_ptr(),
            name.as_ptr(),
            value.as_ptr().cast(),
            value.len(),
            0,
            0,
        ) != 0
        {
            tracing::warn!("quarantine {path}: {}", std::io::Error::last_os_error());
        }
    }
}

#[cfg(all(not(target_os = "macos"), not(test)))]
fn quarantine(_path: &str) {}

#[cfg(test)]
mod tests {
    use super::*;
    use tdlib_rs::enums::TextEntityType;
    use tdlib_rs::types::{FormattedText, TextEntity};

    use crate::testutil::{self, BASE_DATE};
    use crate::ui::image::{Entry, Images};
    use crate::ui::tests::render;
    use crossterm::event::{KeyCode, KeyModifiers};
    use std::time::Duration;

    fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    fn ch(c: char) -> KeyEvent {
        let mods = if c.is_ascii_uppercase() {
            KeyModifiers::SHIFT
        } else {
            KeyModifiers::NONE
        };
        key(KeyCode::Char(c), mods)
    }

    #[tokio::test]
    async fn focus_keys_in_enhanced_and_legacy_terminals() {
        let (mut app, _rx) = testutil::app(Images::disabled(), true);
        app.current = Some(View {
            chat_id: 10,
            topic: None,
        });
        app.on_key(key(KeyCode::Char('i'), KeyModifiers::CONTROL));
        assert_eq!(app.focus, Focus::Composer);
        app.on_key(key(KeyCode::Char('g'), KeyModifiers::CONTROL));
        assert_eq!(app.focus, Focus::Tree);
        app.on_key(key(KeyCode::Char('t'), KeyModifiers::CONTROL));
        assert_eq!(app.focus, Focus::Messages);
        app.on_key(key(KeyCode::Char('l'), KeyModifiers::CONTROL));
        assert_eq!(app.focus, Focus::Composer);

        let (mut legacy, _rx2) = testutil::app(Images::disabled(), false);
        legacy.current = Some(View {
            chat_id: 10,
            topic: None,
        });
        legacy.focus = Focus::Tree;
        legacy.on_key(key(KeyCode::Backspace, KeyModifiers::NONE));
        legacy.on_key(key(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(legacy.focus, Focus::Tree, "Backspace/Tab never move focus");
        legacy.on_key(key(KeyCode::Char('i'), KeyModifiers::ALT));
        assert_eq!(legacy.focus, Focus::Composer);
    }

    #[tokio::test]
    async fn tree_moves_jumps_to_unread_and_opens() {
        let (mut app, _rx) = testutil::app(Images::disabled(), true);
        app.focus = Focus::Tree;
        // Rows: All chats, Family, Muted news, Saved Messages.
        app.on_key(ch('j'));
        assert_eq!(app.tree.selected, Some(NodeId::Chat(ListKey::Main, 10)));
        app.on_key(ch('J'));
        assert_eq!(
            app.tree.selected,
            Some(NodeId::Chat(ListKey::Main, 11)),
            "next unread"
        );
        app.on_key(ch('G'));
        assert_eq!(app.tree.selected, Some(NodeId::Chat(ListKey::Main, 12)));
        app.on_key(key(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            app.current,
            Some(View {
                chat_id: 12,
                topic: None
            })
        );
        assert_eq!(app.focus, Focus::Composer);
        app.focus = Focus::Tree;
        app.on_key(ch('_'));
        assert_eq!(app.tree.selected, Some(NodeId::List(ListKey::Main)));
        assert!(app.tree.expanded.is_empty());
    }

    #[tokio::test]
    async fn picker_filters_and_opens() {
        let (mut app, _rx) = testutil::app(Images::disabled(), true);
        app.on_key(key(KeyCode::Char('k'), KeyModifiers::CONTROL));
        assert!(matches!(app.modal, Modal::Picker(_)));
        for c in "svd".chars() {
            app.on_key(ch(c));
        }
        app.on_key(key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(app.modal, Modal::None));
        assert_eq!(
            app.current,
            Some(View {
                chat_id: 12,
                topic: None
            })
        );
        assert_eq!(
            app.tree.selected,
            Some(NodeId::Chat(ListKey::Main, 12)),
            "picker reveals the chat in the tree"
        );
    }

    #[tokio::test]
    async fn composer_up_edits_last_own_message_and_esc_cancels() {
        let (mut app, mut rx) = testutil::app(Images::disabled(), true);
        app.current = Some(View {
            chat_id: 10,
            topic: None,
        });
        app.focus = Focus::Composer;
        app.on_key(key(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(app.composer.mode, Mode::Edit(102));
        // Formatted: TDLib writes the markdown.
        let ev = wait_for(&mut rx, |e| matches!(e, AppEvent::EditReady { .. })).await;
        app.on_app_event(ev);
        assert_eq!(app.composer.text(), "**Great** thanks");
        app.on_key(key(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.composer.mode, Mode::Normal);
        assert!(app.composer.is_empty());
        // Typing goes into the composer, even letters bound elsewhere.
        for c in "jk".chars() {
            app.on_key(ch(c));
        }
        assert_eq!(app.composer.text(), "jk");
        app.on_key(key(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.composer.is_empty(), "esc clears text");
        app.on_key(key(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(
            app.focus,
            Focus::Messages,
            "esc on empty composer leaves it"
        );
    }

    #[tokio::test]
    async fn messages_select_reply_and_delete_confirm() {
        let (mut app, _rx) = testutil::app(Images::disabled(), true);
        app.current = Some(View {
            chat_id: 10,
            topic: None,
        });
        app.focus = Focus::Messages;
        app.on_key(ch('k'));
        assert_eq!(
            app.msgs.selected,
            Some(102),
            "k from nothing selects the newest"
        );
        app.on_key(ch('s'));
        assert_eq!(
            app.msgs.selected,
            Some(100),
            "s jumps to the replied message"
        );
        app.on_key(ch('d'));
        assert!(matches!(
            app.modal,
            Modal::ConfirmDelete {
                chat_id: 10,
                message_id: 100,
                ..
            }
        ));
        app.on_key(ch('n'));
        assert!(matches!(app.modal, Modal::None));
        app.on_key(ch('R'));
        assert_eq!(app.composer.mode, Mode::Reply(100));
        assert_eq!(app.focus, Focus::Composer);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn photo_downloads_encode_and_render_inline() {
        let dir = std::env::temp_dir().join(format!("telegordo-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let png = dir.join("p.png");
        ::image::RgbImage::from_fn(64, 48, |x, y| {
            ::image::Rgb([(x * 4) as u8, (y * 5) as u8, 200])
        })
        .save(&png)
        .unwrap();
        let path = png.to_string_lossy().into_owned();
        let images = Images {
            picker: Some(ratatui_image::picker::Picker::halfblocks()),
            max_cols: 40,
            max_rows: 12,
            entries: Default::default(),
        };
        let (mut app, mut rx) = testutil::app(images, true);
        app.store.insert_message(testutil::message(
            103,
            10,
            2,
            BASE_DATE + 120,
            testutil::photo_content(77, &path, 64, 48),
            None,
        ));
        app.store.files.insert(77, testutil::file(77, &path, true));
        app.current = Some(View {
            chat_id: 10,
            topic: None,
        });

        let before = render(&mut app, 90, 30);
        assert!(before.contains("[Photo]"), "placeholder while loading");
        app.after_draw();
        let ev = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                match rx.recv().await {
                    Some(e @ AppEvent::ImageReady { .. }) => break e,
                    Some(_) => continue,
                    None => panic!("channel closed"),
                }
            }
        })
        .await
        .expect("image encoded");
        app.on_app_event(ev);
        assert!(matches!(
            app.images.entries.get(&77),
            Some(Entry::Ready { .. })
        ));
        let after = render(&mut app, 90, 30);
        println!("{after}");
        assert!(
            after.contains('▀') || after.contains('▄'),
            "halfblock pixels drawn"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn tdlib_parses_the_documented_markdown() {
        use tdlib_rs::enums::TextEntityType as T;
        let tg = testutil::shared_tg();
        let src = "**b** __i__ ~~s~~ ||p|| `c` [t](https://example.com)\n```rust\nfn x() {}\n```";
        let f = tokio::time::timeout(Duration::from_secs(10), tg.formatted(src.into(), true))
            .await
            .expect("parse_markdown answered before login");
        let kinds: Vec<&T> = f.entities.iter().map(|e| &e.r#type).collect();
        for want in [
            "Bold",
            "Italic",
            "Strikethrough",
            "Spoiler",
            "Code",
            "TextUrl",
            "PreCode",
        ] {
            assert!(
                kinds.iter().any(|k| format!("{k:?}").starts_with(want)),
                "missing {want} in {kinds:?}"
            );
        }
        assert!(f.text.starts_with("b i s p c t"));
        // Editing round-trip: the markdown TDLib writes parses back to the same entities.
        let md = tg.markdown_of(f.clone()).await.unwrap();
        assert!(md.entities.is_empty(), "all of it is markdown: {md:?}");
        let again = tg.formatted(md.text, true).await;
        assert_eq!(again.text, f.text);
        assert_eq!(again.entities, f.entities);
    }

    fn entity(offset: i32, length: i32, r#type: TextEntityType) -> TextEntity {
        TextEntity {
            offset,
            length,
            r#type,
        }
    }

    fn own_text_message(id: i64, text: FormattedText) -> Message {
        let content = serde_json::json!({"@type": "messageText", "text": text,
            "link_preview": null, "link_preview_options": null});
        testutil::message(id, 10, 1, BASE_DATE + 200, content, None)
    }

    async fn wait_for(
        rx: &mut UnboundedReceiver<AppEvent>,
        want: impl Fn(&AppEvent) -> bool,
    ) -> AppEvent {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                match rx.recv().await {
                    Some(e) if want(&e) => break e,
                    Some(_) => continue,
                    None => panic!("channel closed"),
                }
            }
        })
        .await
        .expect("event in time")
    }

    fn enter() -> KeyEvent {
        key(KeyCode::Enter, KeyModifiers::NONE)
    }

    fn type_str(app: &mut App, s: &str) {
        for c in s.chars() {
            app.on_key(ch(c));
        }
    }

    #[tokio::test]
    async fn sends_queue_in_order_and_a_failure_refills_the_composer() {
        let (mut app, _rx) = testutil::app(Images::disabled(), true);
        let mut ops = testutil::capture_outbox(&mut app);
        app.current = Some(view(10, None));
        app.focus = Focus::Composer;
        type_str(&mut app, "one");
        app.on_key(enter());
        type_str(&mut app, "two");
        app.on_key(enter());
        let sent: Vec<String> = testutil::drain(&mut ops)
            .into_iter()
            .map(|op| match op {
                Op::Send {
                    body: Body::Typed { text, .. },
                    ..
                } => text,
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        assert_eq!(sent, vec!["one", "two"]);
        assert!(app.composer.is_empty());

        // Telegram refused the reply: the text and the reply mode come back.
        let draft = Draft {
            text: "too long".into(),
            mode: Mode::Reply(100),
            edit: None,
        };
        app.on_app_event(AppEvent::SendFailed {
            view: view(10, None),
            draft: draft.clone(),
            error: "telegram error 400: MESSAGE_TOO_LONG".into(),
        });
        assert_eq!(app.composer.text(), "too long");
        assert_eq!(app.composer.mode, Mode::Reply(100));
        assert!(app.status.as_ref().unwrap().0.contains("MESSAGE_TOO_LONG"));

        // Not over newer typing.
        app.composer.set_text("newer");
        app.on_app_event(AppEvent::SendFailed {
            view: view(10, None),
            draft,
            error: "x".into(),
        });
        assert_eq!(app.composer.text(), "newer");
    }

    #[tokio::test]
    async fn unchanged_edit_sends_nothing_and_plain_text_is_not_reparsed() {
        let (mut app, _rx) = testutil::app(Images::disabled(), true);
        let mut ops = testutil::capture_outbox(&mut app);
        let plain = "see __init__.py, 2**10** and a_b__c";
        app.store.insert_message(own_text_message(
            103,
            FormattedText {
                text: plain.into(),
                entities: vec![],
            },
        ));
        app.current = Some(view(10, None));
        app.focus = Focus::Composer;

        app.on_key(key(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(app.composer.mode, Mode::Edit(103));
        assert_eq!(app.composer.text(), plain, "plain text, as is");
        app.on_key(enter());
        assert!(testutil::drain(&mut ops).is_empty(), "no-op edit");
        assert_eq!(app.composer.mode, Mode::Normal);

        app.on_key(key(KeyCode::Up, KeyModifiers::NONE));
        app.on_key(key(KeyCode::End, KeyModifiers::NONE));
        app.on_key(ch('!'));
        app.on_key(enter());
        let ops = testutil::drain(&mut ops);
        let [
            Op::Edit {
                message_id: 103,
                body,
                ..
            },
        ] = ops.as_slice()
        else {
            panic!("expected one edit, got {ops:?}");
        };
        let sent = body.clone().resolve(&testutil::shared_tg()).await;
        assert_eq!(sent.text, format!("{plain}!"));
        assert!(sent.entities.is_empty());
        // What the old path did: markdown parsing changes the text.
        let reparsed = Body::Typed {
            text: format!("{plain}!"),
            markdown: true,
        }
        .resolve(&testutil::shared_tg())
        .await;
        assert_ne!(reparsed.text, sent.text);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn formatted_edit_uses_tdlib_markdown_and_warns_before_dropping_formatting() {
        let (mut app, mut rx) = testutil::app(Images::disabled(), true);
        let mut ops = testutil::capture_outbox(&mut app);
        // "hi Bob, look": Bob is a mention without a username, "look" underlined, "hi" bold.
        app.store.insert_message(own_text_message(
            103,
            FormattedText {
                text: "hi Bob, look".into(),
                entities: vec![
                    entity(0, 2, TextEntityType::Bold),
                    entity(
                        3,
                        3,
                        TextEntityType::MentionName(tdlib_rs::types::TextEntityTypeMentionName {
                            user_id: 42,
                        }),
                    ),
                    entity(8, 4, TextEntityType::Underline),
                ],
            },
        ));
        app.current = Some(view(10, None));
        app.focus = Focus::Composer;
        app.on_key(key(KeyCode::Up, KeyModifiers::NONE));
        let ev = wait_for(&mut rx, |e| matches!(e, AppEvent::EditReady { .. })).await;
        app.on_app_event(ev);
        assert_eq!(app.composer.text(), "**hi** Bob, look");
        let lossy = &app.composer.edit.as_ref().unwrap().lossy;
        assert!(lossy.contains(&"mentions") && lossy.contains(&"underline"));

        app.on_key(key(KeyCode::End, KeyModifiers::NONE));
        app.on_key(ch('!'));
        app.on_key(enter());
        assert!(
            testutil::drain(&mut ops).is_empty(),
            "first enter only warns"
        );
        let status = app.status.as_ref().unwrap().0.clone();
        assert!(status.contains("mentions, underline"), "{status}");
        assert_eq!(app.composer.text(), "**hi** Bob, look!");
        app.on_key(enter());
        let ops = testutil::drain(&mut ops);
        assert!(
            matches!(
                ops.as_slice(),
                [Op::Edit { body: Body::Typed { text, markdown: true }, .. }]
                    if text == "**hi** Bob, look!"
            ),
            "{ops:?}"
        );
    }

    #[tokio::test]
    async fn opening_a_file_that_can_run_code_asks_first() {
        let dir = std::env::temp_dir().join(format!("telegordo-open-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (jar, pdf, exe) = (dir.join("x.jar"), dir.join("x.pdf"), dir.join("tool"));
        for p in [&jar, &pdf, &exe] {
            std::fs::write(p, "x").unwrap();
        }
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        assert!(can_run_code(&jar) && can_run_code(&exe));
        assert!(can_run_code(std::path::Path::new("Report.PDF.Command")));
        assert!(!can_run_code(&pdf));

        let (mut app, _rx) = testutil::app(Images::disabled(), true);
        let opened = || OPENED.with(|o| std::mem::take(&mut *o.borrow_mut()));
        let open = |app: &mut App, p: &std::path::Path| {
            let p = p.to_string_lossy();
            app.on_app_event(AppEvent::File {
                purpose: FilePurpose::Open,
                result: Ok(testutil::file(5, &p, true)),
            });
        };
        open(&mut app, &pdf);
        assert!(matches!(app.modal, Modal::None));
        assert_eq!(opened(), vec![pdf.to_string_lossy().to_string()]);

        open(&mut app, &jar);
        assert!(matches!(app.modal, Modal::ConfirmOpen { .. }));
        assert!(render(&mut app, 90, 20).contains("x.jar can run code. Open anyway? y / N"));
        app.on_key(enter());
        assert!(matches!(app.modal, Modal::None));
        assert!(opened().is_empty(), "enter cancels");
        open(&mut app, &jar);
        app.on_key(ch('y'));
        assert_eq!(opened(), vec![jar.to_string_lossy().to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn view(chat_id: i64, topic: Option<i32>) -> View {
        View { chat_id, topic }
    }

    #[tokio::test]
    async fn ctrl_d_asks_before_logging_out() {
        let (mut app, _rx) = testutil::app(Images::disabled(), true);
        let mut ops = testutil::capture_outbox(&mut app);
        app.current = Some(view(10, None));
        app.focus = Focus::Composer;
        let ctrl_d = key(KeyCode::Char('d'), KeyModifiers::CONTROL);
        for cancel in [
            ch('n'),
            key(KeyCode::Enter, KeyModifiers::NONE),
            key(KeyCode::Esc, KeyModifiers::NONE),
            ch('x'),
        ] {
            app.on_key(ctrl_d);
            assert!(matches!(app.modal, Modal::ConfirmLogout), "composer focus");
            assert!(!app.logging_out);
            app.on_key(cancel);
            assert!(matches!(app.modal, Modal::None), "{cancel:?} cancels");
            assert!(!app.logging_out);
        }
        assert!(app.composer.is_empty(), "the modal took the keys");
        assert!(testutil::drain(&mut ops).is_empty());

        app.on_key(ctrl_d);
        let s = render(&mut app, 90, 20);
        assert!(s.contains("Log out of Telegram and delete the local session? y / N"));
        app.on_key(ch('y'));
        assert!(app.logging_out);
        assert_eq!(testutil::drain(&mut ops), vec![Op::LogOut]);
        app.on_key(ctrl_d);
        assert!(matches!(app.modal, Modal::None), "no second logout");
        assert!(testutil::drain(&mut ops).is_empty());
    }

    fn new_message_in_topic(id: i64, topic: i32) -> Update {
        let mut m = testutil::message(
            id,
            10,
            2,
            BASE_DATE + 500,
            testutil::text_content("hey there"),
            None,
        );
        m.topic_id = Some(enums::MessageTopic::Forum(
            tdlib_rs::types::MessageTopicForum {
                forum_topic_id: topic,
            },
        ));
        Update::NewMessage(tdlib_rs::types::UpdateNewMessage { message: m })
    }

    #[tokio::test]
    async fn only_messages_of_the_open_topic_are_marked_read() {
        let (mut app, _rx) = testutil::app(Images::disabled(), true);
        let mut ops = testutil::capture_outbox(&mut app);
        app.current = Some(view(10, Some(5)));
        app.on_update(new_message_in_topic(200, 6));
        assert!(
            testutil::drain(&mut ops).is_empty(),
            "a message in another topic stays unread"
        );
        app.on_update(new_message_in_topic(201, 5));
        assert_eq!(
            testutil::drain(&mut ops),
            vec![Op::ViewMessages {
                chat_id: 10,
                ids: vec![201]
            }]
        );
        // The whole-chat view shows every topic.
        app.current = Some(view(10, None));
        app.on_update(new_message_in_topic(202, 6));
        assert_eq!(testutil::drain(&mut ops).len(), 1);
    }

    #[tokio::test]
    async fn delete_says_for_whom_and_enter_never_confirms() {
        let (mut app, _rx) = testutil::app(Images::disabled(), true);
        let mut ops = testutil::capture_outbox(&mut app);
        app.current = Some(view(10, None));
        app.focus = Focus::Messages;
        app.msgs.selected = Some(101);
        let rights = |app: &mut App, for_everyone, only_for_self| {
            app.on_app_event(AppEvent::DeleteRights {
                chat_id: 10,
                message_id: 101,
                result: Ok(DeleteRights {
                    for_everyone,
                    only_for_self,
                }),
            })
        };
        let delete = |revoke| Op::Delete {
            chat_id: 10,
            message_id: 101,
            revoke,
        };

        app.on_key(ch('d'));
        app.on_key(ch('y'));
        assert!(
            matches!(app.modal, Modal::ConfirmDelete { rights: None, .. }),
            "y waits until TDLib says what is allowed"
        );
        rights(&mut app, true, true);
        let s = render(&mut app, 90, 20);
        assert!(
            s.contains("Delete for everyone? y / N · m: only for me"),
            "{s}"
        );
        app.on_key(key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(app.modal, Modal::None), "enter cancels");
        assert!(testutil::drain(&mut ops).is_empty());

        app.on_key(ch('d'));
        rights(&mut app, true, true);
        app.on_key(ch('m'));
        assert_eq!(testutil::drain(&mut ops), vec![delete(false)]);

        app.on_key(ch('d'));
        rights(&mut app, true, true);
        app.on_key(ch('y'));
        assert_eq!(testutil::drain(&mut ops), vec![delete(true)]);

        app.on_key(ch('d'));
        rights(&mut app, false, true);
        assert!(render(&mut app, 90, 20).contains("Delete for me? y / N"));
        app.on_key(ch('y'));
        assert_eq!(testutil::drain(&mut ops), vec![delete(false)]);
    }

    #[test]
    fn file_names_stay_inside_the_download_dir() {
        assert_eq!(sanitize("", "file_7"), "file_7");
        assert_eq!(sanitize(".", "file_7"), "file_7");
        assert_eq!(sanitize("..", "file_7"), "file_7");
        assert_eq!(sanitize("a/b", "file_7"), "a_b");
        assert_eq!(sanitize("../../x", "f"), ".._.._x");
        assert_eq!(sanitize("a\u{1b}b\n", "f"), "ab");
        let dir = std::env::temp_dir().join(format!("telegordo-names-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["", "..", "a/b"] {
            let p = unique_path(&dir.join(sanitize(name, "file_7")));
            assert_eq!(p.parent(), Some(dir.as_path()), "{name:?} gave {p:?}");
            std::fs::write(&p, "x").unwrap();
        }
        // "" and ".." both became file_7; the second got a number.
        assert!(dir.join("file_7").exists() && dir.join("file_7 (1)").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn auth_state_is_logged_without_its_payload() {
        let s: AuthorizationState = serde_json::from_value(serde_json::json!({
            "@type": "authorizationStateWaitPassword",
            "password_hint": "my 4711 hint",
            "has_recovery_email_address": true,
            "has_passport_data": false,
            "recovery_email_address_pattern": "a***@example.com",
        }))
        .unwrap();
        let name = auth_state_name(&s);
        assert_eq!(name, "wait_password");
        assert!(!name.contains("4711") && !name.contains("example"));
    }

    fn texts(ids: std::ops::RangeInclusive<i64>) -> Vec<Message> {
        ids.map(|id| {
            testutil::message(
                id,
                10,
                2,
                BASE_DATE + 60,
                testutil::text_content(&format!("msg {id}")),
                None,
            )
        })
        .collect()
    }

    #[tokio::test]
    async fn jumping_far_back_marks_the_gap_and_loads_it_from_the_newer_side() {
        let (mut app, mut rx) = testutil::app(Images::disabled(), true);
        app.current = Some(view(10, None));
        // The newest messages (100..=102) are loaded; the reply target 50 is not.
        app.on_app_event(AppEvent::Around {
            view: view(10, None),
            target: 50,
            result: Ok(texts(45..=55)),
        });
        assert_eq!(app.msgs.selected, Some(50));
        assert_eq!(app.store.gaps_of(10), vec![(55, 100)]);
        // Show the bottom, where the marker sits above the newer block.
        app.msgs.selected = None;
        app.msgs.scroll = 0;
        let s = render(&mut app, 100, 30);
        assert!(s.contains("older messages not loaded"), "{s}");
        assert_eq!(app.wanted_gaps, vec![100]);

        app.after_draw();
        assert!(app.store.gap_mut(10, 100).unwrap().loading);
        let ev = wait_for(&mut rx, |e| matches!(e, AppEvent::GapPage { .. })).await;
        let AppEvent::GapPage { newer_end, .. } = &ev else {
            unreachable!()
        };
        assert_eq!(*newer_end, 100, "pages backwards from the newer side");

        // TDLib answers with part of the hole, then with the rest.
        app.on_app_event(AppEvent::GapPage {
            chat_id: 10,
            newer_end: 100,
            result: Ok(texts(80..=99)),
        });
        assert_eq!(app.store.gaps_of(10), vec![(55, 80)]);
        app.on_app_event(AppEvent::GapPage {
            chat_id: 10,
            newer_end: 80,
            result: Ok(texts(50..=79)),
        });
        assert!(app.store.gaps_of(10).is_empty());
        assert!(!render(&mut app, 100, 30).contains("older messages not loaded"));
    }

    #[tokio::test]
    async fn muted_scope_silences_default_chats() {
        let (mut app, _rx) = testutil::app(Images::disabled(), true);
        app.store.chats.get_mut(&10).unwrap().use_default_mute_for = true;
        let m = app.store.message(10, 100).unwrap().clone();
        assert!(!app.store.message_muted(&m));
        app.on_app_event(AppEvent::ScopeMute {
            scope: enums::NotificationSettingsScope::GroupChats,
            mute_for: 3600,
        });
        assert!(app.store.message_muted(&m));
        // The tree dims it and the folder badge stops counting it.
        let rows = chats_tree::rows(&app.store, &app.tree);
        let family = rows.iter().find(|r| r.label == "Family").unwrap();
        assert!(family.muted);
        assert_eq!(rows[0].unread, 0, "Family was the only unread unmuted chat");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn saving_copies_off_the_event_loop_and_reports_the_final_path() {
        let dir = std::env::temp_dir().join(format!("telegordo-save-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("src.bin");
        std::fs::write(&src, "payload").unwrap();
        std::fs::write(dir.join("report.pdf"), "taken").unwrap();
        let (mut app, mut rx) = testutil::app(Images::disabled(), true);
        app.on_app_event(AppEvent::File {
            purpose: FilePurpose::Save(dir.join("report.pdf")),
            result: Ok(testutil::file(5, &src.to_string_lossy(), true)),
        });
        // The handler only queued the copy; the result comes back as an event.
        let AppEvent::Status(s) = wait_for(&mut rx, |e| matches!(e, AppEvent::Status(_))).await
        else {
            unreachable!()
        };
        let saved = dir.join("report (1).pdf");
        assert_eq!(s, format!("saved to {}", saved.display()));
        assert_eq!(std::fs::read_to_string(&saved).unwrap(), "payload");
        assert_eq!(
            std::fs::read_to_string(dir.join("report.pdf")).unwrap(),
            "taken"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_new_image_size_requests_a_new_encode() {
        use ratatui_image::picker::Picker;
        let picker = Picker::halfblocks();
        let encode = |picker: &Picker, cols, rows| {
            picker
                .new_protocol(
                    ::image::DynamicImage::new_rgb8(16, 16),
                    ratatui::layout::Size::new(cols, rows),
                    ratatui_image::Resize::Fit(None),
                )
                .unwrap()
        };
        let proto = encode(&picker, 10, 5);
        let resized = encode(&picker, 8, 4);
        let images = Images {
            picker: Some(picker),
            max_cols: 40,
            max_rows: 12,
            entries: Default::default(),
        };
        let (mut app, _rx) = testutil::app(images, true);
        app.store
            .files
            .insert(77, testutil::file(77, "/nonexistent.png", true));
        app.images.entries.insert(
            77,
            Entry::Ready {
                proto,
                cols: 10,
                rows: 5,
                resizing: None,
            },
        );
        let wanted = |cols, rows| Wanted {
            file_id: 77,
            cols,
            rows,
        };
        app.wanted = vec![wanted(10, 5)];
        app.after_draw();
        assert!(
            matches!(app.images.entries[&77], Entry::Ready { .. }),
            "same size: kept"
        );
        app.wanted = vec![wanted(8, 4)];
        app.after_draw();
        assert!(matches!(
            app.images.entries[&77],
            Entry::Ready {
                cols: 10,
                rows: 5,
                resizing: Some((8, 4)),
                ..
            }
        ));
        assert!(
            app.images.protocol(77).is_some(),
            "old image stays meanwhile"
        );
        app.wanted = vec![wanted(8, 4)];
        app.after_draw();
        assert!(
            matches!(
                app.images.entries[&77],
                Entry::Ready {
                    resizing: Some((8, 4)),
                    ..
                }
            ),
            "no second request for the same size"
        );
        // A late result for the old size does not replace the new request.
        app.on_app_event(AppEvent::ImageReady {
            wanted: wanted(10, 5),
            result: Err("stale".into()),
        });
        assert!(matches!(
            app.images.entries[&77],
            Entry::Ready {
                resizing: Some((8, 4)),
                ..
            }
        ));
        app.on_app_event(AppEvent::ImageReady {
            wanted: wanted(8, 4),
            result: Ok(resized),
        });
        assert!(matches!(
            app.images.entries[&77],
            Entry::Ready {
                cols: 8,
                rows: 4,
                resizing: None,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn quit_closes_after_queued_sends_and_slow_ops_do_not_block() {
        let (mut app, _rx) = testutil::app(Images::disabled(), true);
        let mut ops = testutil::capture_outbox(&mut app);
        app.current = Some(view(10, None));
        app.focus = Focus::Composer;
        type_str(&mut app, "bye");
        app.on_key(enter());
        app.on_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL));
        let ops = testutil::drain(&mut ops);
        assert!(
            matches!(ops.as_slice(), [Op::Send { .. }, Op::Close]),
            "{ops:?}"
        );
        // Edits, deletes and logout wait for the server, so they run apart.
        assert!(!outbox::waits_for_server(&ops[0]));
        assert!(!outbox::waits_for_server(&Op::Close));
        assert!(outbox::waits_for_server(&Op::LogOut));
        assert!(outbox::waits_for_server(&Op::Delete {
            chat_id: 1,
            message_id: 1,
            revoke: true
        }));
    }

    fn auth_state(name: &str) -> Update {
        serde_json::from_value(serde_json::json!({
            "@type": "updateAuthorizationState",
            "authorization_state": {"@type": name},
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn a_failed_logout_keeps_the_session() {
        let dir = std::env::temp_dir().join(format!("telegordo-logout-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (mut app, _rx) = testutil::app(Images::disabled(), true);
        app.params.database_dir = dir.clone();
        app.params.key_store = crate::tg::KeyStore::File(dir.join("missing.key"));
        app.on_key(key(KeyCode::Char('d'), KeyModifiers::CONTROL));
        app.on_key(ch('y'));
        assert!(app.logging_out);
        app.on_app_event(AppEvent::LogOutFailed("network".into()));
        assert!(!app.logging_out, "logout can be tried again");
        app.on_key(key(KeyCode::Char('d'), KeyModifiers::CONTROL));
        assert!(matches!(app.modal, Modal::ConfirmLogout));
        app.on_key(ch('y'));
        // Even with the flag set, a close Telegram did not start as a logout
        // (no authorizationStateLoggingOut) keeps the database.
        app.on_update(auth_state("authorizationStateClosed"));
        assert!(dir.exists(), "session kept");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_failed_send_in_a_chat_left_behind_becomes_its_draft() {
        let (mut app, _rx) = testutil::app(Images::disabled(), true);
        app.open_view(view(12, None));
        app.on_app_event(AppEvent::SendFailed {
            view: view(10, None),
            draft: Draft {
                text: "lost?".into(),
                mode: Mode::Normal,
                edit: None,
            },
            error: "telegram error 403: CHAT_WRITE_FORBIDDEN".into(),
        });
        assert!(app.composer.is_empty(), "chat 12's composer untouched");
        assert!(app.status.as_ref().unwrap().0.contains("draft"));
        app.open_view(view(10, None));
        assert_eq!(app.composer.text(), "lost?");
    }

    #[tokio::test]
    async fn eviction_drops_encoded_images_of_forgotten_messages() {
        let (mut app, _rx) = testutil::app(Images::disabled(), true);
        app.store.insert_message(testutil::message(
            103,
            10,
            2,
            BASE_DATE + 120,
            testutil::photo_content(77, "/x.png", 64, 48),
            None,
        ));
        app.images
            .entries
            .insert(77, Entry::Requested { cols: 4, rows: 2 });
        app.images
            .entries
            .insert(78, Entry::Requested { cols: 4, rows: 2 });
        app.evict();
        assert!(app.images.entries.contains_key(&77));
        assert!(!app.images.entries.contains_key(&78), "no message uses 78");
    }

    #[tokio::test]
    async fn a_failed_gap_load_is_retried_when_the_chat_is_reopened() {
        let (mut app, _rx) = testutil::app(Images::disabled(), true);
        app.store.insert_page(10, texts(45..=55), None);
        app.store.gap_mut(10, 100).unwrap().failed = true;
        app.open_view(view(10, None));
        assert!(!app.store.gap_mut(10, 100).unwrap().failed);
    }

    #[tokio::test]
    async fn background_chats_keep_a_bounded_cache() {
        let (mut app, _rx) = testutil::app(Images::disabled(), true);
        app.current = Some(view(12, None));
        // Chat 10 was opened before; chat 11 never was.
        app.store.history.insert(view(10, None), Default::default());
        for id in 1000..1000 + KEEP_BACKGROUND as i64 + 100 {
            for chat in [10, 11] {
                let m =
                    testutil::message(id, chat, 2, BASE_DATE, testutil::text_content("x"), None);
                app.on_update(Update::NewMessage(tdlib_rs::types::UpdateNewMessage {
                    message: m,
                }));
            }
        }
        assert_eq!(app.store.messages[&10].len(), KEEP_BACKGROUND);
        assert_eq!(app.store.messages[&11].len(), KEEP_UNOPENED);
        let newest = 1000 + KEEP_BACKGROUND as i64 + 99;
        assert!(app.store.message(11, newest).is_some(), "keeps the newest");
    }

    #[test]
    fn osc52_encodes_the_text() {
        assert_eq!(osc52("hi"), "\x1b]52;c;aGk=\x07");
    }

    #[tokio::test]
    async fn without_focus_events_other_chats_still_notify() {
        let (mut app, _rx) = testutil::app(Images::disabled(), true);
        app.current = Some(view(10, None));
        let other = testutil::message(300, 12, 2, BASE_DATE, testutil::text_content("hi"), None);
        // `would_notify` mirrors `maybe_notify`'s decision.
        assert!(
            app.would_notify(&other),
            "no focus events: notify for other chats"
        );
        let open = app.store.message(10, 100).unwrap().clone();
        assert!(!app.would_notify(&open), "but not for the open chat");
        app.on_terminal(Event::FocusGained);
        assert!(!app.would_notify(&other), "focused, when_unfocused = true");
        app.on_terminal(Event::FocusLost);
        assert!(app.would_notify(&other));
    }

    #[tokio::test]
    async fn drafts_survive_switching_chats() {
        let (mut app, _rx) = testutil::app(Images::disabled(), true);
        app.open_view(view(10, None));
        type_str(&mut app, "half typed");
        app.open_view(view(12, None));
        assert!(app.composer.is_empty());
        app.open_view(view(10, None));
        assert_eq!(app.composer.text(), "half typed");
    }
}
