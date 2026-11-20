//! One long-lived task for user-initiated TDLib requests. Requests TDLib
//! answers locally (sends, read receipts, chat open/close, close) run in the
//! order they were made, so two quick sends reach TDLib in order and `close`
//! comes after everything queued before it. Requests that wait for the
//! server (edits, deletes, logout) run on their own, so a slow connection
//! does not hold up the queue. A failed send or edit hands its text back.

use tdlib_rs::functions;
use tdlib_rs::types::FormattedText;
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};

use crate::app::AppEvent;
use crate::tg::{Tg, View};
use crate::ui::composer::Draft;

/// The text of a send or edit.
#[derive(Debug, Clone, PartialEq)]
pub enum Body {
    /// Sent exactly as given, without markdown parsing.
    Exact(FormattedText),
    /// Composer text, parsed as markdown when `markdown` is set.
    Typed { text: String, markdown: bool },
}

impl Body {
    pub async fn resolve(self, tg: &Tg) -> FormattedText {
        match self {
            Body::Exact(ft) => ft,
            Body::Typed { text, markdown } => tg.formatted(text, markdown).await,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Op {
    Send {
        view: View,
        reply_to: Option<i64>,
        body: Body,
        /// Put back into the composer if the request fails.
        draft: Draft,
    },
    Edit {
        view: View,
        message_id: i64,
        body: Body,
        draft: Draft,
    },
    /// `viewMessages(force_read = true)`.
    ViewMessages {
        chat_id: i64,
        ids: Vec<i64>,
    },
    OpenChat(i64),
    CloseChat(i64),
    /// `close`: last in line, after the sends queued before it.
    Close,
    Delete {
        chat_id: i64,
        message_id: i64,
        revoke: bool,
    },
    LogOut,
}

/// Starts the outbox task and returns its queue.
pub fn spawn(tg: Tg, tx: UnboundedSender<AppEvent>) -> UnboundedSender<Op> {
    let (op_tx, mut op_rx) = unbounded_channel::<Op>();
    tokio::spawn(async move {
        while let Some(op) = op_rx.recv().await {
            if waits_for_server(&op) {
                let (tg, tx) = (tg.clone(), tx.clone());
                tokio::spawn(async move { run(&tg, &tx, op).await });
            } else {
                run(&tg, &tx, op).await;
            }
        }
    });
    op_tx
}

/// Requests whose answer needs a server round trip, which can take minutes
/// on a bad connection. (`sendMessage` answers at once with a pending message.)
pub fn waits_for_server(op: &Op) -> bool {
    matches!(op, Op::Edit { .. } | Op::Delete { .. } | Op::LogOut)
}

async fn run(tg: &Tg, tx: &UnboundedSender<AppEvent>, op: Op) {
    match op {
        Op::Send {
            view,
            reply_to,
            body,
            draft,
        } => {
            let text = body.resolve(tg).await;
            if let Err(e) = tg.send_text(view, reply_to, text).await {
                let _ = tx.send(AppEvent::SendFailed {
                    view,
                    draft,
                    error: format!("{e:#}"),
                });
            }
        }
        Op::Edit {
            view,
            message_id,
            body,
            draft,
        } => {
            let text = body.resolve(tg).await;
            if let Err(e) = tg.edit_text(view.chat_id, message_id, text).await {
                let _ = tx.send(AppEvent::SendFailed {
                    view,
                    draft,
                    error: format!("{e:#}"),
                });
            }
        }
        Op::ViewMessages { chat_id, ids } => {
            if let Err(e) = functions::view_messages(chat_id, ids, None, true, tg.client_id).await {
                tracing::debug!("view_messages: {}", e.message);
            }
        }
        Op::OpenChat(chat_id) => {
            if let Err(e) = functions::open_chat(chat_id, tg.client_id).await {
                tracing::debug!("open_chat: {}", e.message);
            }
        }
        Op::CloseChat(chat_id) => {
            if let Err(e) = functions::close_chat(chat_id, tg.client_id).await {
                tracing::debug!("close_chat: {}", e.message);
            }
        }
        Op::Delete {
            chat_id,
            message_id,
            revoke,
        } => {
            let status = match tg.delete(chat_id, message_id, revoke).await {
                Ok(true) => "deleted for everyone".to_string(),
                Ok(false) => "deleted for you".to_string(),
                Err(e) => format!("delete failed: {e:#}"),
            };
            let _ = tx.send(AppEvent::Status(status));
        }
        Op::Close => {
            if let Err(e) = functions::close(tg.client_id).await {
                tracing::debug!("close: {}", e.message);
            }
        }
        Op::LogOut => {
            if let Err(e) = functions::log_out(tg.client_id).await {
                let _ = tx.send(AppEvent::LogOutFailed(e.message));
            }
        }
    }
}
