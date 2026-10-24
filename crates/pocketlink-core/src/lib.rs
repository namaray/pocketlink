// SPDX-License-Identifier: GPL-3.0-or-later

//! Client for the Pocketlink daemon, plus the types it shares with the app.
//!
//! The daemon (`pocketlink-daemon`) owns the connections to phones. Apps talk
//! to it on the D-Bus session bus; state comes across as JSON so both sides can
//! evolve without breaking the bus interface.

use std::path::Path;

use futures::{Stream, StreamExt};
use serde::{Deserialize, Serialize};

pub use zbus::Error;
pub type Result<T> = zbus::Result<T>;

pub const BUS_NAME: &str = "io.github.namaray.Pocketlink";
pub const OBJECT_PATH: &str = "/io/github/namaray/Pocketlink";
pub const INTERFACE: &str = "io.github.namaray.Pocketlink1";

/// Packet types, used here as capability names.
pub mod caps {
    pub const SHARE: &str = "kdeconnect.share.request";
    pub const CLIPBOARD: &str = "kdeconnect.clipboard";
    pub const RING: &str = "kdeconnect.findmyphone.request";
    pub const PING: &str = "kdeconnect.ping";
    pub const SFTP: &str = "kdeconnect.sftp.request";
    pub const NOTIFICATION_REPLY: &str = "kdeconnect.notification.reply";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Device {
    pub id: String,
    pub name: String,
    pub kind: DeviceKind,
    pub reachable: bool,
    pub pair: PairState,
    /// The link it is reachable over right now.
    pub transport: Option<Transport>,
    pub address: Option<String>,
    /// Packet types the device accepts from us.
    pub accepts: Vec<String>,
    pub battery: Option<Battery>,
    pub signal: Option<Signal>,
    pub notifications: Vec<Notification>,
}

impl Device {
    pub fn paired(&self) -> bool {
        self.pair == PairState::Paired
    }

    /// Whether the device is connected, paired, and accepts `capability`.
    pub fn can(&self, capability: &str) -> bool {
        self.reachable && self.paired() && self.accepts.iter().any(|c| c == capability)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DeviceKind {
    Phone,
    Tablet,
    Desktop,
    Laptop,
    Tv,
    Unknown,
}

impl DeviceKind {
    pub fn parse(kind: &str) -> Self {
        match kind {
            "phone" | "smartphone" => Self::Phone,
            "tablet" => Self::Tablet,
            "desktop" => Self::Desktop,
            "laptop" => Self::Laptop,
            "tv" => Self::Tv,
            _ => Self::Unknown,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum PairState {
    Unpaired,
    /// We asked; waiting for them to accept. `code` should match on both screens.
    Requested { code: String },
    /// They asked; waiting for us to accept.
    RequestedByPeer { code: String },
    Paired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Transport {
    Wifi,
    Bluetooth,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Battery {
    /// Percent, 0–100.
    pub charge: u8,
    pub charging: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Signal {
    /// Bars, 0–4.
    pub strength: u8,
    /// For example "LTE" or "5G".
    pub network: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Notification {
    pub id: String,
    pub app: String,
    pub title: String,
    pub text: String,
    pub time: Option<i64>,
    pub dismissable: bool,
    /// Set when the notification accepts a typed reply.
    pub reply_id: Option<String>,
    pub actions: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Transfer {
    pub id: u64,
    pub device_id: String,
    pub direction: Direction,
    /// File name, or "3 files" for a batch.
    pub name: String,
    pub done: u64,
    pub total: u64,
    pub state: TransferState,
    /// Where a received file was saved.
    pub path: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    Incoming,
    Outgoing,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "lowercase")]
pub enum TransferState {
    Active,
    Done,
    Failed { error: String },
    Cancelled,
}

/// Something happened in the daemon.
#[derive(Debug, Clone)]
pub enum Event {
    /// Device list or device state changed. Re-read with [`Client::devices`].
    Changed,
    /// A transfer started, progressed or finished.
    Transfer(Transfer),
    /// The phone sent text or a link to our clipboard.
    ClipboardReceived { device_id: String, text: String },
    /// A device wants to pair.
    PairRequest { device_id: String, name: String, code: String },
}

#[zbus::proxy(
    interface = "io.github.namaray.Pocketlink1",
    default_service = "io.github.namaray.Pocketlink",
    default_path = "/io/github/namaray/Pocketlink"
)]
trait Daemon {
    fn devices(&self) -> zbus::Result<String>;
    fn transfers(&self) -> zbus::Result<String>;
    fn refresh(&self) -> zbus::Result<()>;
    fn add_address(&self, address: &str) -> zbus::Result<()>;
    fn pair(&self, id: &str) -> zbus::Result<()>;
    fn accept_pair(&self, id: &str) -> zbus::Result<()>;
    fn reject_pair(&self, id: &str) -> zbus::Result<()>;
    fn unpair(&self, id: &str) -> zbus::Result<()>;
    fn send_files(&self, id: &str, paths: &[&str]) -> zbus::Result<()>;
    fn send_text(&self, id: &str, text: &str) -> zbus::Result<()>;
    fn send_clipboard(&self, id: &str, text: &str) -> zbus::Result<()>;
    fn ring(&self, id: &str) -> zbus::Result<()>;
    fn ping(&self, id: &str) -> zbus::Result<()>;
    fn browse(&self, id: &str) -> zbus::Result<()>;
    fn reply_notification(&self, id: &str, reply_id: &str, message: &str) -> zbus::Result<()>;
    fn dismiss_notification(&self, id: &str, notification_id: &str) -> zbus::Result<()>;
    fn cancel_transfer(&self, transfer_id: u64) -> zbus::Result<()>;

    #[zbus(signal)]
    fn changed(&self) -> zbus::Result<()>;
    #[zbus(signal)]
    fn transfer_changed(&self, transfer: &str) -> zbus::Result<()>;
    #[zbus(signal)]
    fn clipboard_received(&self, id: &str, text: &str) -> zbus::Result<()>;
    #[zbus(signal)]
    fn pair_requested(&self, id: &str, name: &str, code: &str) -> zbus::Result<()>;
}

#[derive(Debug, Clone)]
pub struct Client {
    proxy: DaemonProxy<'static>,
}

impl Client {
    /// Connect to the daemon, starting it through D-Bus activation if needed.
    pub async fn connect() -> Result<Self> {
        let conn = zbus::Connection::session().await?;
        Ok(Self {
            proxy: DaemonProxy::new(&conn).await?,
        })
    }

    /// Paired devices, plus unpaired ones that are reachable right now.
    pub async fn devices(&self) -> Result<Vec<Device>> {
        json(&self.proxy.devices().await?)
    }

    pub async fn transfers(&self) -> Result<Vec<Transfer>> {
        json(&self.proxy.transfers().await?)
    }

    /// Announce ourselves on the network again.
    pub async fn refresh(&self) -> Result<()> {
        self.proxy.refresh().await
    }

    /// Reach a device by IP when broadcasts don't get through (VPNs, guest Wi-Fi).
    pub async fn add_address(&self, address: &str) -> Result<()> {
        self.proxy.add_address(address).await
    }

    pub async fn pair(&self, id: &str) -> Result<()> {
        self.proxy.pair(id).await
    }

    pub async fn accept_pair(&self, id: &str) -> Result<()> {
        self.proxy.accept_pair(id).await
    }

    pub async fn reject_pair(&self, id: &str) -> Result<()> {
        self.proxy.reject_pair(id).await
    }

    pub async fn unpair(&self, id: &str) -> Result<()> {
        self.proxy.unpair(id).await
    }

    /// Send files. Progress arrives as [`Event::Transfer`].
    pub async fn send_files<P: AsRef<Path>>(&self, id: &str, files: &[P]) -> Result<()> {
        let paths: Vec<String> = files
            .iter()
            .map(|p| p.as_ref().to_string_lossy().into_owned())
            .collect();
        let paths: Vec<&str> = paths.iter().map(String::as_str).collect();
        self.proxy.send_files(id, &paths).await
    }

    /// Send a link (opens on the phone) or text.
    pub async fn send_text(&self, id: &str, text: &str) -> Result<()> {
        self.proxy.send_text(id, text).await
    }

    /// Put `text` on the phone's clipboard.
    pub async fn send_clipboard(&self, id: &str, text: &str) -> Result<()> {
        self.proxy.send_clipboard(id, text).await
    }

    /// Make the phone ring loudly, even when on silent.
    pub async fn ring(&self, id: &str) -> Result<()> {
        self.proxy.ring(id).await
    }

    pub async fn ping(&self, id: &str) -> Result<()> {
        self.proxy.ping(id).await
    }

    /// Mount the phone's storage and open it in the file manager.
    pub async fn browse(&self, id: &str) -> Result<()> {
        self.proxy.browse(id).await
    }

    pub async fn reply_notification(&self, id: &str, reply_id: &str, message: &str) -> Result<()> {
        self.proxy.reply_notification(id, reply_id, message).await
    }

    pub async fn dismiss_notification(&self, id: &str, notification_id: &str) -> Result<()> {
        self.proxy.dismiss_notification(id, notification_id).await
    }

    pub async fn cancel_transfer(&self, transfer_id: u64) -> Result<()> {
        self.proxy.cancel_transfer(transfer_id).await
    }

    /// Everything the daemon announces, as [`Event`]s.
    pub async fn events(&self) -> Result<impl Stream<Item = Event> + use<>> {
        let changed = self.proxy.receive_changed().await?.map(|_| Event::Changed);
        let transfers = self
            .proxy
            .receive_transfer_changed()
            .await?
            .filter_map(|s| async move {
                let args = s.args().ok()?;
                serde_json::from_str(args.transfer()).ok().map(Event::Transfer)
            });
        let clipboard = self
            .proxy
            .receive_clipboard_received()
            .await?
            .filter_map(|s| async move {
                let args = s.args().ok()?;
                Some(Event::ClipboardReceived {
                    device_id: args.id().to_string(),
                    text: args.text().to_string(),
                })
            });
        let pairing = self
            .proxy
            .receive_pair_requested()
            .await?
            .filter_map(|s| async move {
                let args = s.args().ok()?;
                Some(Event::PairRequest {
                    device_id: args.id().to_string(),
                    name: args.name().to_string(),
                    code: args.code().to_string(),
                })
            });

        Ok(futures::stream::select_all([
            changed.boxed(),
            transfers.boxed(),
            clipboard.boxed(),
            pairing.boxed(),
        ]))
    }
}

fn json<T: for<'de> Deserialize<'de>>(text: &str) -> Result<T> {
    serde_json::from_str(text).map_err(|e| Error::Failure(format!("bad reply from daemon: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pair_state_json() {
        let state = PairState::RequestedByPeer { code: "ABCD1234".into() };
        let text = serde_json::to_string(&state).unwrap();
        assert_eq!(text, r#"{"state":"requested_by_peer","code":"ABCD1234"}"#);
        assert_eq!(serde_json::from_str::<PairState>(&text).unwrap(), state);
    }

    #[test]
    fn device_capabilities() {
        let device = Device {
            id: "x".into(),
            name: "Phone".into(),
            kind: DeviceKind::Phone,
            reachable: true,
            pair: PairState::Paired,
            transport: Some(Transport::Wifi),
            address: None,
            accepts: vec![caps::RING.into()],
            battery: None,
            signal: None,
            notifications: vec![],
        };
        assert!(device.can(caps::RING));
        assert!(!device.can(caps::SHARE));
    }
}
