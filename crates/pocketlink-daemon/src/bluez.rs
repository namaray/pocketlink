// SPDX-License-Identifier: GPL-3.0-or-later

//! Bluetooth through BlueZ, on the system bus.
//!
//! We register an RFCOMM profile for the KDE Connect service UUID. BlueZ then
//! publishes the SDP record (so phones can find and connect to us) and hands
//! us every connected socket through `org.bluez.Profile1.NewConnection`, both
//! for connections the phone makes and ones we ask for with `ConnectProfile`.
//!
//! Only devices already paired with this computer over Bluetooth can connect.

use std::collections::{HashMap, HashSet};
use std::os::fd::OwnedFd;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pocketlink_proto::bluetooth::{self, BtConnection, Role, SERVICE_NAME, SERVICE_UUID};
use pocketlink_proto::{DeviceInfo, Identity};
use tokio::sync::{mpsc, watch};
use zbus::fdo;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};

use crate::daemon::Daemon;

const PROFILE_PATH: &str = "/io/github/namaray/Pocketlink/bluetooth";
const RECONNECT_EVERY: Duration = Duration::from_secs(30);

type Pending = Arc<Mutex<HashSet<OwnedObjectPath>>>;

struct Profile {
    /// Devices we asked BlueZ to connect to; their sockets come back as `NewConnection` too.
    outgoing: Pending,
    sockets: mpsc::UnboundedSender<(OwnedObjectPath, OwnedFd, Role)>,
}

#[zbus::interface(name = "org.bluez.Profile1")]
impl Profile {
    fn release(&self) {
        tracing::info!("BlueZ released our Bluetooth profile");
    }

    fn new_connection(
        &self,
        device: OwnedObjectPath,
        fd: zbus::zvariant::OwnedFd,
        _properties: HashMap<String, OwnedValue>,
    ) -> fdo::Result<()> {
        let ours = self.outgoing.lock().expect("not poisoned").remove(&device);
        let role = if ours { Role::Connected } else { Role::Accepted };
        let _ = self.sockets.send((device, fd.into(), role));
        Ok(())
    }

    fn request_disconnection(&self, device: OwnedObjectPath) {
        tracing::debug!(%device, "BlueZ asked us to disconnect");
    }
}

#[zbus::proxy(interface = "org.bluez.ProfileManager1", default_service = "org.bluez", default_path = "/org/bluez")]
trait ProfileManager {
    fn register_profile(
        &self,
        profile: &ObjectPath<'_>,
        uuid: &str,
        options: HashMap<&str, Value<'_>>,
    ) -> zbus::Result<()>;
}

#[zbus::proxy(interface = "org.bluez.Device1", default_service = "org.bluez")]
trait Device {
    fn connect_profile(&self, uuid: &str) -> zbus::Result<()>;
}

/// Start Bluetooth support. Gives up quietly when there's no adapter or BlueZ.
pub async fn start(
    daemon: Arc<Daemon>,
    identity: Identity,
    me: watch::Receiver<DeviceInfo>,
    links: mpsc::Sender<(BtConnection, String)>,
) {
    if let Err(err) = run(daemon, identity, me, links).await {
        tracing::warn!(%err, "Bluetooth unavailable");
    }
}

async fn run(
    daemon: Arc<Daemon>,
    identity: Identity,
    me: watch::Receiver<DeviceInfo>,
    links: mpsc::Sender<(BtConnection, String)>,
) -> zbus::Result<()> {
    let system = zbus::Connection::system().await?;
    let outgoing: Pending = Arc::default();
    let (sockets_tx, mut sockets) = mpsc::unbounded_channel();
    system
        .object_server()
        .at(
            PROFILE_PATH,
            Profile {
                outgoing: outgoing.clone(),
                sockets: sockets_tx,
            },
        )
        .await?;

    let mut options = HashMap::new();
    options.insert("Name", Value::from(SERVICE_NAME));
    options.insert("RequireAuthentication", Value::from(true));
    options.insert("RequireAuthorization", Value::from(false));
    options.insert("AutoConnect", Value::from(false));
    ProfileManagerProxy::new(&system)
        .await?
        .register_profile(&ObjectPath::try_from(PROFILE_PATH)?, SERVICE_UUID, options)
        .await?;
    tracing::info!("Bluetooth ready");

    // Every socket BlueZ gives us becomes a link after the identity exchange.
    tokio::spawn(async move {
        while let Some((device, fd, role)) = sockets.recv().await {
            let address = address_of(&device);
            let stream = match socket_stream(fd) {
                Ok(stream) => stream,
                Err(err) => {
                    tracing::warn!(%address, %err, "unusable Bluetooth socket");
                    continue;
                }
            };
            let me = me.borrow().clone();
            let identity = identity.clone();
            let links = links.clone();
            tokio::spawn(async move {
                match bluetooth::handshake(stream, role, &me, &identity).await {
                    Ok(conn) => {
                        let _ = links.send((conn, address)).await;
                    }
                    Err(err) => tracing::debug!(%address, %err, "Bluetooth handshake failed"),
                }
            });
        }
    });

    // Reach out to bonded phones whenever a paired device isn't on Bluetooth yet.
    loop {
        if daemon.wants_bluetooth() {
            connect_bonded(&system, &daemon, &outgoing).await;
        }
        tokio::time::sleep(RECONNECT_EVERY).await;
    }
}

async fn connect_bonded(system: &zbus::Connection, daemon: &Daemon, outgoing: &Pending) {
    let Ok(objects) = fdo::ObjectManagerProxy::builder(system)
        .destination("org.bluez")
        .and_then(|b| b.path("/"))
        .map(|b| b.build())
    else {
        return;
    };
    let Ok(objects) = objects.await else { return };
    let Ok(managed) = objects.get_managed_objects().await else { return };
    let connected = daemon.bluetooth_addresses();

    for (path, interfaces) in managed {
        let Some(props) = interfaces.get("org.bluez.Device1") else { continue };
        let paired = props
            .get("Paired")
            .and_then(|v| bool::try_from(v.clone()).ok())
            .unwrap_or(false);
        let uuids: Vec<String> = props
            .get("UUIDs")
            .and_then(|v| Vec::<String>::try_from(v.clone()).ok())
            .unwrap_or_default();
        let is_phone = props
            .get("Icon")
            .and_then(|v| String::try_from(v.clone()).ok())
            .is_some_and(|icon| icon.starts_with("phone"));
        let address = address_of(&path);
        if !paired || connected.contains(&address) {
            continue;
        }
        // BlueZ only knows the phone offers our service after an SDP lookup,
        // which ConnectProfile does, so phones are worth a try either way.
        if !uuids.iter().any(|u| u == SERVICE_UUID) && !is_phone {
            continue;
        }

        let Ok(device) = DeviceProxy::builder(system).path(path.clone()).map(|b| b.build()) else {
            continue;
        };
        let Ok(device) = device.await else { continue };
        outgoing.lock().expect("not poisoned").insert(path.clone());
        let result = tokio::time::timeout(Duration::from_secs(20), device.connect_profile(SERVICE_UUID)).await;
        match result {
            Ok(Ok(())) => tracing::debug!(%address, "Bluetooth connected"),
            Ok(Err(err)) => {
                outgoing.lock().expect("not poisoned").remove(&path);
                tracing::debug!(%address, %err, "Bluetooth connect failed");
            }
            Err(_) => {
                outgoing.lock().expect("not poisoned").remove(&path);
            }
        }
    }
}

/// An RFCOMM socket from BlueZ, as a tokio stream.
fn socket_stream(fd: OwnedFd) -> std::io::Result<tokio::net::UnixStream> {
    // RFCOMM sockets are plain stream sockets as far as read/write go.
    let std = std::os::unix::net::UnixStream::from(fd);
    std.set_nonblocking(true)?;
    tokio::net::UnixStream::from_std(std)
}

/// `/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF` → `AA:BB:CC:DD:EE:FF`.
fn address_of(path: &ObjectPath<'_>) -> String {
    path.as_str()
        .rsplit('/')
        .next()
        .and_then(|leaf| leaf.strip_prefix("dev_"))
        .map(|mac| mac.replace('_', ":"))
        .unwrap_or_else(|| path.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_from_paths() {
        let path = ObjectPath::try_from("/org/bluez/hci0/dev_1C_52_16_DF_33_AA").unwrap();
        assert_eq!(address_of(&path), "1C:52:16:DF:33:AA");
    }
}
