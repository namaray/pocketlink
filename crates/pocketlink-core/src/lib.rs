// SPDX-License-Identifier: GPL-3.0-or-later

//! Client for the KDE Connect daemon (`kdeconnectd`) on the D-Bus session bus.
//!
//! Pocketlink never speaks the KDE Connect network protocol itself. Pairing,
//! encryption and transport stay inside the daemon; this crate only reads its
//! state and asks it to do things.

use std::path::Path;

use futures::{Stream, StreamExt};
use zbus::message::Type as MessageType;
use zbus::zvariant::OwnedValue;
use zbus::{Connection, MatchRule, MessageStream};

pub use zbus::Error;
pub type Result<T> = zbus::Result<T>;

const SERVICE: &str = "org.kde.kdeconnect";
const ROOT: &str = "/modules/kdeconnect";
const DAEMON_IFACE: &str = "org.kde.kdeconnect.daemon";
const DEVICE_IFACE: &str = "org.kde.kdeconnect.device";

/// A paired phone or tablet, as last reported by the daemon.
#[derive(Debug, Clone, PartialEq)]
pub struct Device {
    pub id: String,
    pub name: String,
    pub kind: DeviceKind,
    pub reachable: bool,
    pub plugins: Vec<String>,
    pub battery: Option<Battery>,
    pub signal: Option<Signal>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceKind {
    Phone,
    Tablet,
    Desktop,
    Laptop,
    Tv,
    Unknown,
}

impl DeviceKind {
    fn parse(kind: &str) -> Self {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Battery {
    /// Charge in percent, 0–100.
    pub charge: u8,
    pub charging: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signal {
    /// Bars, 0–4.
    pub strength: u8,
    /// For example "LTE" or "5G".
    pub network: String,
}

impl Device {
    pub fn has_plugin(&self, plugin: &str) -> bool {
        self.plugins.iter().any(|p| p == plugin)
    }
}

/// Something changed on the daemon side.
#[derive(Debug, Clone)]
pub enum Event {
    /// The phone sent us a file; `url` points at where the daemon saved it.
    ShareReceived { device_id: String, url: String },
    /// Device list or some device state changed. Re-read with [`Client::devices`].
    Changed,
}

#[derive(Debug, Clone)]
pub struct Client {
    conn: Connection,
}

impl Client {
    pub async fn connect() -> Result<Self> {
        Ok(Self {
            conn: Connection::session().await?,
        })
    }

    /// All paired devices, reachable or not.
    pub async fn devices(&self) -> Result<Vec<Device>> {
        let ids: Vec<String> = self
            .call(ROOT, DAEMON_IFACE, "devices", &(false, true))
            .await?;

        let mut devices = Vec::with_capacity(ids.len());
        for id in ids {
            match self.device(&id).await {
                Ok(device) => devices.push(device),
                Err(err) => tracing::warn!(%id, %err, "skipping device"),
            }
        }
        Ok(devices)
    }

    pub async fn device(&self, id: &str) -> Result<Device> {
        let path = device_path(id);
        let reachable: bool = self.property(&path, DEVICE_IFACE, "isReachable").await?;
        let plugins: Vec<String> = if reachable {
            self.call(&path, DEVICE_IFACE, "loadedPlugins", &())
                .await
                .unwrap_or_default()
        } else {
            Vec::new()
        };

        let has = |p: &str| plugins.iter().any(|x| x == p);
        let battery = if has("kdeconnect_battery") {
            self.battery(id).await.ok()
        } else {
            None
        };
        let signal = if has("kdeconnect_connectivity_report") {
            self.signal(id).await.ok().flatten()
        } else {
            None
        };

        Ok(Device {
            id: id.to_owned(),
            name: self.property(&path, DEVICE_IFACE, "name").await?,
            kind: DeviceKind::parse(&self.property::<String>(&path, DEVICE_IFACE, "type").await?),
            reachable,
            plugins,
            battery,
            signal,
        })
    }

    async fn battery(&self, id: &str) -> Result<Battery> {
        let (path, iface) = plugin(id, "battery");
        let charge: i32 = self.property(&path, &iface, "charge").await?;
        Ok(Battery {
            charge: charge.clamp(0, 100) as u8,
            charging: self.property(&path, &iface, "isCharging").await?,
        })
    }

    async fn signal(&self, id: &str) -> Result<Option<Signal>> {
        let (path, iface) = plugin(id, "connectivity_report");
        let strength: i32 = self.property(&path, &iface, "cellularNetworkStrength").await?;
        if strength < 0 {
            return Ok(None);
        }
        Ok(Some(Signal {
            strength: strength.clamp(0, 4) as u8,
            network: self.property(&path, &iface, "cellularNetworkType").await?,
        }))
    }

    /// Send files to the device. Directories are not supported by the daemon and
    /// should be filtered out by the caller.
    pub async fn send_files<P: AsRef<Path>>(&self, id: &str, files: &[P]) -> Result<()> {
        let urls: Vec<String> = files
            .iter()
            .filter_map(|p| url::Url::from_file_path(p.as_ref()).ok())
            .map(String::from)
            .collect();
        let (path, iface) = plugin(id, "share");
        self.call(&path, &iface, "shareUrls", &(urls,)).await
    }

    /// Send a link or a piece of text. Links open on the phone, text lands on its clipboard.
    pub async fn send_text(&self, id: &str, text: &str) -> Result<()> {
        let (path, iface) = plugin(id, "share");
        if looks_like_url(text) {
            self.call(&path, &iface, "shareUrl", &(text,)).await
        } else {
            self.call(&path, &iface, "shareText", &(text,)).await
        }
    }

    /// Put `text` on the device's clipboard.
    ///
    /// The caller reads the clipboard itself: on Wayland the daemon often can't
    /// see it, but a focused window can.
    pub async fn send_clipboard(&self, id: &str, text: &str) -> Result<()> {
        let (path, iface) = plugin(id, "clipboard");
        self.call(&path, &iface, "sendClipboard", &(text,)).await
    }

    /// Make the phone ring loudly, even when on silent.
    pub async fn ring(&self, id: &str) -> Result<()> {
        let (path, iface) = plugin(id, "findmyphone");
        self.call(&path, &iface, "ring", &()).await
    }

    pub async fn ping(&self, id: &str) -> Result<()> {
        let (path, iface) = plugin(id, "ping");
        self.call(&path, &iface, "sendPing", &()).await
    }

    /// Mount the phone's storage and open it in the file manager.
    pub async fn browse(&self, id: &str) -> Result<bool> {
        let (path, iface) = plugin(id, "sftp");
        self.call(&path, &iface, "startBrowsing", &()).await
    }

    /// Signals from the daemon, mapped to [`Event`]s.
    pub async fn events(&self) -> Result<impl Stream<Item = Event> + use<>> {
        let rule = MatchRule::builder()
            .msg_type(MessageType::Signal)
            .path_namespace(ROOT)?
            .build();
        let stream = MessageStream::for_match_rule(rule, &self.conn, Some(64)).await?;

        Ok(stream.filter_map(|msg| async move {
            let msg = msg.ok()?;
            let header = msg.header();
            let member = header.member()?.as_str();
            let path = header.path()?.as_str();

            if member == "shareReceived" {
                let url: String = msg.body().deserialize().ok()?;
                let device_id = path
                    .strip_prefix("/modules/kdeconnect/devices/")?
                    .split('/')
                    .next()?
                    .to_owned();
                return Some(Event::ShareReceived { device_id, url });
            }
            Some(Event::Changed)
        }))
    }

    async fn call<A, R>(&self, path: &str, iface: &str, method: &str, args: &A) -> Result<R>
    where
        A: serde::Serialize + zbus::zvariant::DynamicType,
        R: for<'d> zbus::zvariant::DynamicDeserialize<'d>,
    {
        let reply = self
            .conn
            .call_method(Some(SERVICE), path, Some(iface), method, args)
            .await?;
        reply.body().deserialize()
    }

    async fn property<T>(&self, path: &str, iface: &str, name: &str) -> Result<T>
    where
        T: TryFrom<OwnedValue>,
        T::Error: Into<zbus::zvariant::Error>,
    {
        let value: OwnedValue = self
            .call(path, "org.freedesktop.DBus.Properties", "Get", &(iface, name))
            .await?;
        T::try_from(value).map_err(|e| Error::Variant(e.into()))
    }
}

fn device_path(id: &str) -> String {
    format!("{ROOT}/devices/{id}")
}

fn plugin(id: &str, name: &str) -> (String, String) {
    (
        format!("{ROOT}/devices/{id}/{name}"),
        format!("{DEVICE_IFACE}.{name}"),
    )
}

fn looks_like_url(text: &str) -> bool {
    let text = text.trim();
    !text.contains(char::is_whitespace)
        && url::Url::parse(text).is_ok_and(|u| matches!(u.scheme(), "http" | "https"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_links() {
        assert!(looks_like_url("https://example.com/a?b=c"));
        assert!(!looks_like_url("hello world"));
        assert!(!looks_like_url("see https://example.com"));
        assert!(!looks_like_url("file:///etc/passwd"));
    }

    #[test]
    fn parses_device_kind() {
        assert_eq!(DeviceKind::parse("phone"), DeviceKind::Phone);
        assert_eq!(DeviceKind::parse("toaster"), DeviceKind::Unknown);
    }
}
