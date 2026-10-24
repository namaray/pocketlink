// SPDX-License-Identifier: GPL-3.0-or-later

//! Desktop notifications (org.freedesktop.Notifications), with clickable actions.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use futures::StreamExt;
use zbus::zvariant::Value;

#[zbus::proxy(
    interface = "org.freedesktop.Notifications",
    default_service = "org.freedesktop.Notifications",
    default_path = "/org/freedesktop/Notifications"
)]
trait Notifications {
    #[allow(clippy::too_many_arguments)]
    fn notify(
        &self,
        app_name: &str,
        replaces_id: u32,
        app_icon: &str,
        summary: &str,
        body: &str,
        actions: &[&str],
        hints: HashMap<&str, Value<'_>>,
        expire_timeout: i32,
    ) -> zbus::Result<u32>;

    fn close_notification(&self, id: u32) -> zbus::Result<()>;

    #[zbus(signal)]
    fn action_invoked(&self, id: u32, action_key: &str) -> zbus::Result<()>;

    #[zbus(signal)]
    fn notification_closed(&self, id: u32, reason: u32) -> zbus::Result<()>;
}

/// What clicking a notification button should do.
#[derive(Debug, Clone)]
pub enum Action {
    AcceptPair(String),
    RejectPair(String),
    Open(std::path::PathBuf),
}

pub struct Note<'a> {
    pub app: &'a str,
    pub icon: &'a str,
    pub summary: &'a str,
    pub body: &'a str,
    /// (button label, what it does). The first one also runs on a plain click.
    pub actions: Vec<(&'a str, Action)>,
    pub urgent: bool,
}

/// Desktop notification id → its buttons, by key.
type Pending = Arc<Mutex<HashMap<u32, Vec<(String, Action)>>>>;

#[derive(Clone)]
pub struct Notifier {
    proxy: Option<NotificationsProxy<'static>>,
    pending: Pending,
}

impl Notifier {
    pub async fn new(conn: &zbus::Connection) -> Self {
        let proxy = NotificationsProxy::new(conn)
            .await
            .inspect_err(|err| tracing::warn!(%err, "desktop notifications unavailable"))
            .ok();
        Self {
            proxy,
            pending: Arc::default(),
        }
    }

    /// Runs `on_action` whenever someone clicks a button on one of our notifications.
    pub async fn listen(&self, on_action: impl Fn(Action) + Send + 'static) {
        let Some(proxy) = self.proxy.clone() else {
            return;
        };
        let (Ok(mut invoked), Ok(mut closed)) = (
            proxy.receive_action_invoked().await,
            proxy.receive_notification_closed().await,
        ) else {
            return;
        };
        let pending = self.pending.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    Some(signal) = invoked.next() => {
                        let Ok(args) = signal.args() else { continue };
                        let action = pending
                            .lock()
                            .expect("not poisoned")
                            .remove(&args.id)
                            .and_then(|actions| {
                                actions.into_iter().find(|(key, _)| key == args.action_key)
                            });
                        if let Some((_, action)) = action {
                            on_action(action);
                        }
                    }
                    Some(signal) = closed.next() => {
                        if let Ok(args) = signal.args() {
                            pending.lock().expect("not poisoned").remove(&args.id);
                        }
                    }
                    else => break,
                }
            }
        });
    }

    /// Returns the notification id, for closing or replacing it later.
    pub async fn show(&self, note: Note<'_>, replaces: u32) -> Option<u32> {
        let proxy = self.proxy.as_ref()?;
        // The first action is also "default", which runs on a plain click.
        let actions: Vec<(String, Action)> = note
            .actions
            .iter()
            .enumerate()
            .map(|(i, (_, action))| {
                let key = if i == 0 { "default".to_owned() } else { format!("a{i}") };
                (key, action.clone())
            })
            .collect();
        let flat: Vec<&str> = actions
            .iter()
            .zip(&note.actions)
            .flat_map(|((key, _), (label, _))| [key.as_str(), *label])
            .collect();
        let mut hints = HashMap::new();
        hints.insert("desktop-entry", Value::from("io.github.namaray.Pocketlink"));
        if note.urgent {
            hints.insert("urgency", Value::from(2u8));
        }

        let id = proxy
            .notify(note.app, replaces, note.icon, note.summary, &escape_markup(note.body), &flat, hints, -1)
            .await
            .inspect_err(|err| tracing::debug!(%err, "notify failed"))
            .ok()?;
        if !actions.is_empty() {
            self.pending.lock().expect("not poisoned").insert(id, actions);
        }
        Some(id)
    }

    pub async fn close(&self, id: u32) {
        if let Some(proxy) = &self.proxy {
            let _ = proxy.close_notification(id).await;
        }
        self.pending.lock().expect("not poisoned").remove(&id);
    }
}

/// Notification bodies may be read as markup; text from the phone must stay plain text.
fn escape_markup(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    #[test]
    fn escapes_markup() {
        assert_eq!(
            super::escape_markup("<a href=\"x\">hi</a> & bye"),
            "&lt;a href=\"x\"&gt;hi&lt;/a&gt; &amp; bye"
        );
    }
}
