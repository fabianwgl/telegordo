//! Desktop notifications for incoming messages.

pub fn send(title: String, body: String) {
    if cfg!(test) {
        // Tests never pop up real notifications.
        return;
    }
    let body: String = body.chars().take(200).collect();
    tokio::task::spawn_blocking(move || {
        let mut n = notify_rust::Notification::new();
        n.summary(&title).body(&body).appname("telegordo");
        if let Err(e) = n.show() {
            tracing::warn!("notification: {e}");
        }
    });
}
