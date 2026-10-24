// SPDX-License-Identifier: GPL-3.0-or-later

//! The D-Bus API apps use. See `pocketlink-core` for the client side.

use std::path::PathBuf;
use std::sync::Arc;

use pocketlink_core::OBJECT_PATH;
use tokio::sync::mpsc;
use zbus::fdo;
use zbus::object_server::SignalEmitter;

use crate::daemon::{Daemon, Reply, Signal};

pub struct Api {
    pub daemon: Arc<Daemon>,
}

fn reply(result: Reply) -> fdo::Result<()> {
    result.map_err(fdo::Error::Failed)
}

#[zbus::interface(name = "io.github.namaray.Pocketlink1")]
impl Api {
    /// JSON array of `pocketlink_core::Device`.
    fn devices(&self) -> String {
        serde_json::to_string(&self.daemon.devices()).expect("devices serialize")
    }

    /// JSON array of `pocketlink_core::Transfer`.
    fn transfers(&self) -> String {
        serde_json::to_string(&self.daemon.transfers()).expect("transfers serialize")
    }

    /// Our own name, as phones see it.
    #[zbus(property)]
    fn name(&self) -> String {
        self.daemon.me().name
    }

    async fn refresh(&self) {
        self.daemon.broadcast().await;
    }

    async fn add_address(&self, address: &str) -> fdo::Result<()> {
        reply(self.daemon.add_address(address).await)
    }

    fn pair(&self, id: &str) -> fdo::Result<()> {
        reply(self.daemon.request_pair(id))
    }

    fn accept_pair(&self, id: &str) -> fdo::Result<()> {
        reply(self.daemon.accept_pair(id))
    }

    fn reject_pair(&self, id: &str) -> fdo::Result<()> {
        reply(self.daemon.reject_pair(id))
    }

    fn unpair(&self, id: &str) -> fdo::Result<()> {
        reply(self.daemon.unpair(id))
    }

    fn send_files(&self, id: &str, paths: Vec<String>) -> fdo::Result<()> {
        reply(
            self.daemon
                .send_files(id, paths.into_iter().map(PathBuf::from).collect()),
        )
    }

    fn send_text(&self, id: &str, text: &str) -> fdo::Result<()> {
        reply(self.daemon.send_text(id, text))
    }

    fn send_clipboard(&self, id: &str, text: &str) -> fdo::Result<()> {
        reply(self.daemon.send_clipboard(id, text))
    }

    fn ring(&self, id: &str) -> fdo::Result<()> {
        reply(self.daemon.ring(id))
    }

    fn ping(&self, id: &str) -> fdo::Result<()> {
        reply(self.daemon.ping(id))
    }

    async fn browse(&self, id: &str) -> fdo::Result<()> {
        reply(self.daemon.browse(id).await)
    }

    fn reply_notification(&self, id: &str, reply_id: &str, message: &str) -> fdo::Result<()> {
        reply(self.daemon.reply_notification(id, reply_id, message))
    }

    fn dismiss_notification(&self, id: &str, notification_id: &str) -> fdo::Result<()> {
        reply(self.daemon.dismiss_notification(id, notification_id))
    }

    fn cancel_transfer(&self, transfer_id: u64) -> fdo::Result<()> {
        reply(self.daemon.cancel_transfer(transfer_id))
    }

    #[zbus(signal)]
    async fn changed(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;

    /// JSON `pocketlink_core::Transfer`.
    #[zbus(signal)]
    async fn transfer_changed(emitter: &SignalEmitter<'_>, transfer: &str) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn clipboard_received(emitter: &SignalEmitter<'_>, id: &str, text: &str) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn pair_requested(
        emitter: &SignalEmitter<'_>,
        id: &str,
        name: &str,
        code: &str,
    ) -> zbus::Result<()>;
}

/// Turns daemon [`Signal`]s into D-Bus signals. Bursts of `Changed` collapse into one.
pub async fn emit_signals(conn: zbus::Connection, mut rx: mpsc::UnboundedReceiver<Signal>) {
    let Ok(emitter) = SignalEmitter::new(&conn, OBJECT_PATH) else {
        return;
    };
    while let Some(first) = rx.recv().await {
        let mut batch = vec![first];
        while let Ok(more) = rx.try_recv() {
            batch.push(more);
        }
        let mut changed = false;
        for signal in batch {
            let result = match signal {
                Signal::Changed => {
                    changed = true;
                    Ok(())
                }
                Signal::Transfer(t) => {
                    let json = serde_json::to_string(&t).expect("transfer serializes");
                    Api::transfer_changed(&emitter, &json).await
                }
                Signal::Clipboard { id, text } => Api::clipboard_received(&emitter, &id, &text).await,
                Signal::PairRequested { id, name, code } => {
                    Api::pair_requested(&emitter, &id, &name, &code).await
                }
            };
            if let Err(err) = result {
                tracing::debug!(%err, "signal emit failed");
            }
        }
        if changed {
            let _ = Api::changed(&emitter).await;
        }
    }
}
