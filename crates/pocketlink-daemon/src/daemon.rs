// SPDX-License-Identifier: GPL-3.0-or-later

//! Devices, links, pairing and what each packet type means.

use std::collections::{BTreeMap, HashMap};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use pocketlink_core::{self as api, Direction, TransferState};
use pocketlink_proto::lan::{self, Lan};
use pocketlink_proto::payload::{self, PayloadServer};
use pocketlink_proto::{Connection, DeviceInfo, Identity, Packet, TlsConfigs, identity, packet};
use rustls_pki_types::CertificateDer;
use serde_json::{Value, json};
use tokio::io::{AsyncWriteExt, BufReader, BufWriter, ReadHalf, WriteHalf};
use tokio::sync::{mpsc, watch};

use crate::notify::{Action, Note, Notifier};
use crate::store::{Store, Trusted};

/// Packet types we handle. The phone only sends what we list here.
pub const INCOMING: &[&str] = &[
    "kdeconnect.ping",
    "kdeconnect.battery",
    "kdeconnect.connectivity_report",
    "kdeconnect.share.request",
    "kdeconnect.share.request.update",
    "kdeconnect.clipboard",
    "kdeconnect.clipboard.connect",
    "kdeconnect.notification",
    "kdeconnect.sftp",
];

/// Packet types we send. The phone loads the matching plugins.
pub const OUTGOING: &[&str] = &[
    "kdeconnect.ping",
    "kdeconnect.share.request",
    "kdeconnect.clipboard",
    "kdeconnect.findmyphone.request",
    "kdeconnect.sftp.request",
    "kdeconnect.notification.request",
    "kdeconnect.notification.reply",
    "kdeconnect.notification.action",
];

const PAIR_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_CLOCK_SKEW_SECS: i64 = 30 * 60;
const PROGRESS_INTERVAL: Duration = Duration::from_millis(250);

/// Something apps should hear about. Sent as D-Bus signals.
#[derive(Debug, Clone)]
pub enum Signal {
    Changed,
    Transfer(api::Transfer),
    Clipboard { id: String, text: String },
    PairRequested { id: String, name: String, code: String },
}

pub type Reply<T = ()> = Result<T, String>;

pub struct Daemon {
    identity: Identity,
    tls: TlsConfigs,
    store: Store,
    downloads: PathBuf,
    me: watch::Sender<DeviceInfo>,
    state: Mutex<State>,
    lan: OnceLock<Lan>,
    notifier: OnceLock<Notifier>,
    signals: mpsc::UnboundedSender<Signal>,
    next_id: AtomicU64,
}

#[derive(Default)]
struct State {
    devices: HashMap<String, DeviceState>,
    trusted: HashMap<String, Trusted>,
    transfers: BTreeMap<u64, TransferEntry>,
    mounts: HashMap<String, PathBuf>,
}

struct DeviceState {
    info: DeviceInfo,
    cert: Option<CertificateDer<'static>>,
    link: Option<Link>,
    pair: Pair,
    battery: Option<api::Battery>,
    signal: Option<api::Signal>,
    notifications: BTreeMap<String, api::Notification>,
    /// Phone notification id → desktop notification id.
    popups: HashMap<String, u32>,
    pair_popup: Option<u32>,
}

impl DeviceState {
    fn new(info: DeviceInfo) -> Self {
        Self {
            info,
            cert: None,
            link: None,
            pair: Pair::Unpaired,
            battery: None,
            signal: None,
            notifications: BTreeMap::new(),
            popups: HashMap::new(),
            pair_popup: None,
        }
    }
}

#[derive(Clone)]
struct Link {
    generation: u64,
    peer: SocketAddr,
    tx: mpsc::UnboundedSender<Packet>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pair {
    Unpaired,
    /// We asked, at this Unix time.
    Requested(i64),
    /// They asked, at this Unix time.
    RequestedByPeer(i64),
    Paired,
}

struct TransferEntry {
    info: api::Transfer,
    abort: Option<tokio::task::AbortHandle>,
    last_emit: Instant,
}

impl Daemon {
    pub fn new(
        store: Store,
        identity: Identity,
        tls: TlsConfigs,
        me: watch::Sender<DeviceInfo>,
        signals: mpsc::UnboundedSender<Signal>,
    ) -> Self {
        let trusted = store.trusted();
        let mut devices = HashMap::new();
        for (id, t) in &trusted {
            let mut device = DeviceState::new(DeviceInfo {
                id: id.clone(),
                name: t.name.clone(),
                kind: t.kind.clone(),
                protocol_version: t.protocol_version,
                incoming: Vec::new(),
                outgoing: Vec::new(),
            });
            device.cert = t.cert();
            device.pair = Pair::Paired;
            devices.insert(id.clone(), device);
        }
        Self {
            identity,
            tls,
            store,
            downloads: crate::store::download_dir(),
            me,
            state: Mutex::new(State {
                devices,
                trusted,
                ..State::default()
            }),
            lan: OnceLock::new(),
            notifier: OnceLock::new(),
            signals,
            next_id: AtomicU64::new(1),
        }
    }

    pub fn set_lan(&self, lan: Lan) {
        let _ = self.lan.set(lan);
    }

    pub fn set_notifier(&self, notifier: Notifier) {
        let _ = self.notifier.set(notifier);
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().expect("state lock poisoned")
    }

    fn emit(&self, signal: Signal) {
        let _ = self.signals.send(signal);
    }

    fn next(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    // ---- What apps see ---------------------------------------------------

    pub fn devices(&self) -> Vec<api::Device> {
        let st = self.lock();
        let mut out: Vec<api::Device> = st
            .devices
            .values()
            .filter(|d| d.pair == Pair::Paired || d.link.is_some())
            .map(|d| self.snapshot(d))
            .collect();
        out.sort_by(|a, b| {
            b.paired()
                .cmp(&a.paired())
                .then(b.reachable.cmp(&a.reachable))
                .then(a.name.cmp(&b.name))
        });
        out
    }

    fn snapshot(&self, d: &DeviceState) -> api::Device {
        let code = |ts: i64| {
            d.cert
                .as_ref()
                .and_then(|c| identity::verification_key(&self.identity.cert, c, ts))
                .unwrap_or_default()
        };
        api::Device {
            id: d.info.id.clone(),
            name: d.info.name.clone(),
            kind: api::DeviceKind::parse(&d.info.kind),
            reachable: d.link.is_some(),
            pair: match d.pair {
                Pair::Unpaired => api::PairState::Unpaired,
                Pair::Requested(ts) => api::PairState::Requested { code: code(ts) },
                Pair::RequestedByPeer(ts) => api::PairState::RequestedByPeer { code: code(ts) },
                Pair::Paired => api::PairState::Paired,
            },
            transport: d.link.as_ref().map(|_| api::Transport::Wifi),
            address: d.link.as_ref().map(|l| l.peer.ip().to_canonical().to_string()),
            accepts: d.info.incoming.clone(),
            battery: d.battery,
            signal: d.signal.clone(),
            notifications: d.notifications.values().rev().cloned().collect(),
        }
    }

    pub fn transfers(&self) -> Vec<api::Transfer> {
        self.lock().transfers.values().map(|t| t.info.clone()).collect()
    }

    pub fn me(&self) -> DeviceInfo {
        self.me.borrow().clone()
    }

    // ---- Links -------------------------------------------------------------

    pub async fn run(self: Arc<Self>, mut connections: mpsc::Receiver<Connection>) {
        while let Some(conn) = connections.recv().await {
            self.on_connection(conn);
        }
    }

    fn on_connection(self: &Arc<Self>, conn: Connection) {
        let Connection {
            info,
            cert,
            peer,
            reader,
            writer,
        } = conn;
        let id = info.id.clone();
        let generation = self.next();
        let (tx, rx) = mpsc::unbounded_channel();

        let paired = {
            let mut st = self.lock();
            if let Some(trusted) = st.trusted.get(&id)
                && trusted.cert().as_ref() != Some(&cert) {
                    tracing::warn!(%id, name = %info.name, "certificate changed since pairing; refusing");
                    return;
                }
            let device = st
                .devices
                .entry(id.clone())
                .or_insert_with(|| DeviceState::new(info.clone()));
            device.info = info.clone();
            device.cert = Some(cert);
            device.link = Some(Link {
                generation,
                peer,
                tx,
            });
            let paired = device.pair == Pair::Paired;
            if let Some(t) = st.trusted.get_mut(&id).filter(|t| t.name != info.name) {
                t.name = info.name.clone();
                self.store.save_trusted(&st.trusted);
            }
            paired
        };

        tokio::spawn(write_loop(writer, rx));
        tokio::spawn(self.clone().read_loop(id.clone(), generation, reader));
        tracing::info!(%id, name = %info.name, %peer, paired, "device connected");
        if paired {
            self.on_ready(&id);
        }
        self.emit(Signal::Changed);
    }

    async fn read_loop(self: Arc<Self>, id: String, generation: u64, mut reader: BufReader<ReadHalf<lan::Stream>>) {
        loop {
            match lan::read_packet(&mut reader).await {
                Ok(Some(packet)) => self.on_packet(&id, generation, packet).await,
                Ok(None) => break,
                Err(err) => {
                    tracing::debug!(%id, %err, "link read failed");
                    break;
                }
            }
        }
        self.on_disconnect(&id, generation).await;
    }

    async fn on_disconnect(&self, id: &str, generation: u64) {
        let (popup, mount) = {
            let mut st = self.lock();
            let Some(device) = st.devices.get_mut(id) else { return };
            if device.link.as_ref().map(|l| l.generation) != Some(generation) {
                return; // An older link; a newer one replaced it.
            }
            device.link = None;
            if matches!(device.pair, Pair::Requested(_) | Pair::RequestedByPeer(_)) {
                device.pair = Pair::Unpaired;
            }
            let popup = device.pair_popup.take();
            if device.pair != Pair::Paired {
                st.devices.remove(id);
            }
            (popup, st.mounts.remove(id))
        };
        tracing::info!(%id, "device disconnected");
        if let Some(popup) = popup {
            self.close_popup(popup).await;
        }
        if let Some(mount) = mount {
            unmount(&mount).await;
        }
        self.emit(Signal::Changed);
    }

    fn send(&self, id: &str, packet: Packet) -> Reply {
        let st = self.lock();
        let device = st.devices.get(id).ok_or("Unknown device")?;
        let link = device.link.as_ref().ok_or_else(|| format!("{} is offline", device.info.name))?;
        link.tx
            .send(packet)
            .map_err(|_| format!("{} disconnected", device.info.name))
    }

    /// Like `send`, but only to paired devices.
    fn send_paired(&self, id: &str, packet: Packet) -> Reply {
        {
            let st = self.lock();
            let device = st.devices.get(id).ok_or("Unknown device")?;
            if device.pair != Pair::Paired {
                return Err(format!("{} isn't paired", device.info.name));
            }
        }
        self.send(id, packet)
    }

    /// A paired device is connected and ready.
    fn on_ready(&self, id: &str) {
        let _ = self.send(
            id,
            Packet::new("kdeconnect.notification.request", json!({ "request": true })),
        );
    }

    pub async fn broadcast(&self) {
        if let Some(lan) = self.lan.get() {
            lan.broadcast().await;
        }
    }

    pub async fn add_address(&self, address: &str) -> Reply {
        let ip: IpAddr = address
            .trim()
            .parse()
            .map_err(|_| format!("“{address}” isn't an IP address"))?;
        let mut settings = self.store.settings();
        if !settings.addresses.contains(&ip.to_string()) {
            settings.addresses.push(ip.to_string());
            self.store.save_settings(&settings);
        }
        if let Some(lan) = self.lan.get() {
            lan.announce_to(SocketAddr::new(ip, lan::UDP_PORT)).await;
        }
        Ok(())
    }

    // ---- Pairing -----------------------------------------------------------

    pub fn request_pair(self: &Arc<Self>, id: &str) -> Reply {
        let ts = now_secs();
        {
            let mut st = self.lock();
            let device = st.devices.get_mut(id).ok_or("Unknown device")?;
            match device.pair {
                Pair::Paired | Pair::Requested(_) => return Ok(()),
                Pair::RequestedByPeer(_) => {
                    drop(st);
                    return self.accept_pair(id);
                }
                Pair::Unpaired => {}
            }
            if device.link.is_none() {
                return Err(format!("{} is offline", device.info.name));
            }
            device.pair = Pair::Requested(ts);
        }
        self.send(id, Packet::new(packet::PAIR, json!({ "pair": true, "timestamp": ts })))?;
        self.pair_timeout(id, Pair::Requested(ts));
        self.emit(Signal::Changed);
        Ok(())
    }

    pub fn accept_pair(self: &Arc<Self>, id: &str) -> Reply {
        let popup = {
            let mut st = self.lock();
            let device = st.devices.get_mut(id).ok_or("Unknown device")?;
            match device.pair {
                Pair::Paired => return Ok(()),
                Pair::RequestedByPeer(_) => {}
                _ => return Err("There's no pairing request to accept".into()),
            }
            let popup = device.pair_popup.take();
            self.send_locked(&st, id, Packet::new(packet::PAIR, json!({ "pair": true })))?;
            self.mark_paired(&mut st, id);
            popup
        };
        self.after_popup_closed(popup);
        self.on_ready(id);
        self.emit(Signal::Changed);
        Ok(())
    }

    pub fn reject_pair(self: &Arc<Self>, id: &str) -> Reply {
        let popup = {
            let mut st = self.lock();
            let device = st.devices.get_mut(id).ok_or("Unknown device")?;
            if !matches!(device.pair, Pair::Requested(_) | Pair::RequestedByPeer(_)) {
                return Ok(());
            }
            device.pair = Pair::Unpaired;
            device.pair_popup.take()
        };
        let _ = self.send(id, Packet::new(packet::PAIR, json!({ "pair": false })));
        self.after_popup_closed(popup);
        self.emit(Signal::Changed);
        Ok(())
    }

    pub fn unpair(self: &Arc<Self>, id: &str) -> Reply {
        let _ = self.send(id, Packet::new(packet::PAIR, json!({ "pair": false })));
        let mut st = self.lock();
        self.forget(&mut st, id);
        drop(st);
        self.emit(Signal::Changed);
        Ok(())
    }

    fn mark_paired(&self, st: &mut State, id: &str) {
        let Some(device) = st.devices.get_mut(id) else { return };
        let Some(cert) = device.cert.as_ref() else { return };
        device.pair = Pair::Paired;
        let trusted = Trusted {
            name: device.info.name.clone(),
            kind: device.info.kind.clone(),
            protocol_version: device.info.protocol_version,
            certificate: Trusted::encode_cert(cert),
        };
        st.trusted.insert(id.to_owned(), trusted);
        self.store.save_trusted(&st.trusted);
        tracing::info!(%id, "paired");
    }

    fn forget(&self, st: &mut State, id: &str) {
        if st.trusted.remove(id).is_some() {
            self.store.save_trusted(&st.trusted);
        }
        if let Some(device) = st.devices.get_mut(id) {
            device.pair = Pair::Unpaired;
            device.notifications.clear();
            if device.link.is_none() {
                st.devices.remove(id);
            }
        }
    }

    fn send_locked(&self, st: &State, id: &str, packet: Packet) -> Reply {
        let link = st
            .devices
            .get(id)
            .and_then(|d| d.link.as_ref())
            .ok_or("The device is offline")?;
        link.tx.send(packet).map_err(|_| "The device disconnected".to_owned())
    }

    fn pair_timeout(self: &Arc<Self>, id: &str, expected: Pair) {
        let daemon = self.clone();
        let id = id.to_owned();
        tokio::spawn(async move {
            tokio::time::sleep(PAIR_TIMEOUT).await;
            let popup = {
                let mut st = daemon.lock();
                let Some(device) = st.devices.get_mut(&id) else { return };
                if device.pair != expected {
                    return;
                }
                device.pair = Pair::Unpaired;
                device.pair_popup.take()
            };
            let _ = daemon.send(&id, Packet::new(packet::PAIR, json!({ "pair": false })));
            daemon.after_popup_closed(popup);
            daemon.emit(Signal::Changed);
        });
    }

    async fn on_pair(self: &Arc<Self>, id: &str, packet: &Packet) {
        let wants_pair = packet.get_bool("pair").unwrap_or(false);
        let mut ready = false;
        let mut request: Option<(String, String, i64)> = None;
        let mut popup_to_close = None;
        let mut unpaired_name = None;
        {
            let mut st = self.lock();
            let Some(device) = st.devices.get_mut(id) else { return };
            let name = device.info.name.clone();
            if wants_pair {
                match device.pair {
                    Pair::Requested(_) => {
                        self.mark_paired(&mut st, id);
                        ready = true;
                    }
                    Pair::RequestedByPeer(_) => {}
                    Pair::Paired if packet.get_i64("timestamp").is_none() => {}
                    Pair::Paired | Pair::Unpaired => {
                        if device.pair == Pair::Paired {
                            // They forgot us and are pairing from scratch.
                            self.forget(&mut st, id);
                        }
                        let Some(ts) = packet.get_i64("timestamp") else { return };
                        if (ts - now_secs()).abs() > MAX_CLOCK_SKEW_SECS {
                            tracing::warn!(%id, "pairing refused: device clocks are out of sync");
                            return;
                        }
                        let Some(device) = st.devices.get_mut(id) else { return };
                        device.pair = Pair::RequestedByPeer(ts);
                        let code = device
                            .cert
                            .as_ref()
                            .and_then(|c| identity::verification_key(&self.identity.cert, c, ts))
                            .unwrap_or_default();
                        request = Some((name, code, ts));
                    }
                }
            } else {
                match device.pair {
                    Pair::Unpaired => {}
                    Pair::Requested(_) | Pair::RequestedByPeer(_) => {
                        device.pair = Pair::Unpaired;
                        popup_to_close = device.pair_popup.take();
                    }
                    Pair::Paired => {
                        self.forget(&mut st, id);
                        unpaired_name = Some(name);
                    }
                }
            }
        }

        if ready {
            self.on_ready(id);
        }
        if let Some((name, code, ts)) = request {
            self.emit(Signal::PairRequested {
                id: id.to_owned(),
                name: name.clone(),
                code: code.clone(),
            });
            self.pair_timeout(id, Pair::RequestedByPeer(ts));
            let body = format!("Make sure the code {code} is shown on both devices.");
            let summary = format!("{name} wants to pair");
            let popup = self
                .popup(
                    Note {
                        app: "Pocketlink",
                        icon: "phone-symbolic",
                        summary: &summary,
                        body: &body,
                        actions: vec![
                            ("Accept", Action::AcceptPair(id.to_owned())),
                            ("Reject", Action::RejectPair(id.to_owned())),
                        ],
                        urgent: true,
                    },
                    0,
                )
                .await;
            let mut st = self.lock();
            if let Some(device) = st.devices.get_mut(id) {
                device.pair_popup = popup;
            }
        }
        if let Some(popup) = popup_to_close {
            self.close_popup(popup).await;
        }
        if let Some(name) = unpaired_name {
            tracing::info!(%id, "unpaired by the other device");
            self.simple_popup(&format!("{name} unpaired"), "").await;
        }
        self.emit(Signal::Changed);
    }

    // ---- Packets -----------------------------------------------------------

    async fn on_packet(self: &Arc<Self>, id: &str, generation: u64, packet: Packet) {
        let (paired, name) = {
            let st = self.lock();
            let Some(device) = st.devices.get(id) else { return };
            if device.link.as_ref().map(|l| l.generation) != Some(generation) {
                return;
            }
            (device.pair == Pair::Paired, device.info.name.clone())
        };

        if packet.is(packet::PAIR) {
            return self.on_pair(id, &packet).await;
        }
        if !paired {
            tracing::debug!(%id, kind = %packet.kind, "ignoring packet from unpaired device");
            return;
        }

        match packet.kind.as_str() {
            "kdeconnect.ping" => {
                let message = packet.get_str("message").unwrap_or("Ping!");
                self.simple_popup(&name, message).await;
            }
            "kdeconnect.battery" => self.on_battery(id, &name, &packet).await,
            "kdeconnect.connectivity_report" => self.on_connectivity(id, &packet),
            "kdeconnect.share.request" => self.on_share(id, &name, packet).await,
            "kdeconnect.share.request.update" => {}
            "kdeconnect.clipboard" | "kdeconnect.clipboard.connect" => {
                if let Some(text) = packet.get_str("content").filter(|t| !t.is_empty()) {
                    self.emit(Signal::Clipboard {
                        id: id.to_owned(),
                        text: text.to_owned(),
                    });
                }
            }
            "kdeconnect.notification" => self.on_notification(id, &packet).await,
            "kdeconnect.sftp" => self.on_sftp(id, &packet).await,
            other => tracing::debug!(%id, kind = other, "unhandled packet"),
        }
    }

    async fn on_battery(&self, id: &str, name: &str, packet: &Packet) {
        let charge = packet.get_i64("currentCharge").unwrap_or(-1);
        let charging = packet.get_bool("isCharging").unwrap_or(false);
        {
            let mut st = self.lock();
            let Some(device) = st.devices.get_mut(id) else { return };
            device.battery = (0..=100).contains(&charge).then_some(api::Battery {
                charge: charge as u8,
                charging,
            });
        }
        if packet.get_i64("thresholdEvent") == Some(1) {
            self.simple_popup(&format!("{name}'s battery is low"), &format!("{charge}% left"))
                .await;
        }
        self.emit(Signal::Changed);
    }

    fn on_connectivity(&self, id: &str, packet: &Packet) {
        let first = match packet.body.get("signalStrengths") {
            Some(Value::Object(subs)) => subs.values().next().cloned(),
            _ => None,
        };
        let signal = first.and_then(|sub| {
            let strength = sub.get("signalStrength")?.as_i64()?;
            Some(api::Signal {
                strength: strength.clamp(0, 4) as u8,
                network: sub.get("networkType")?.as_str()?.to_owned(),
            })
        });
        if let Some(device) = self.lock().devices.get_mut(id) {
            device.signal = signal;
        }
        self.emit(Signal::Changed);
    }

    async fn on_notification(&self, id: &str, packet: &Packet) {
        let Some(nid) = packet.get_str("id").map(str::to_owned) else { return };

        if packet.get_bool("isCancel") == Some(true) {
            let popup = {
                let mut st = self.lock();
                let Some(device) = st.devices.get_mut(id) else { return };
                device.notifications.remove(&nid);
                device.popups.remove(&nid)
            };
            if let Some(popup) = popup {
                self.close_popup(popup).await;
            }
            self.emit(Signal::Changed);
            return;
        }

        let notification = api::Notification {
            id: nid.clone(),
            app: packet.get_str("appName").unwrap_or_default().to_owned(),
            title: packet.get_str("title").unwrap_or_default().to_owned(),
            text: packet.get_str("text").unwrap_or_default().to_owned(),
            time: packet.get_i64("time"),
            dismissable: packet.get_bool("isClearable").unwrap_or(false),
            reply_id: packet
                .get_str("requestReplyId")
                .filter(|r| !r.is_empty())
                .map(str::to_owned),
            actions: packet.get_str_list("actions"),
        };
        let silent = packet.get_bool("silent").unwrap_or(false);
        let replaces = {
            let mut st = self.lock();
            let Some(device) = st.devices.get_mut(id) else { return };
            device.notifications.insert(nid.clone(), notification.clone());
            device.popups.get(&nid).copied().unwrap_or(0)
        };
        self.emit(Signal::Changed);

        if silent || (notification.title.is_empty() && notification.text.is_empty()) {
            return;
        }
        let summary = if notification.title.is_empty() {
            notification.app.as_str()
        } else {
            notification.title.as_str()
        };
        let popup = self
            .popup(
                Note {
                    app: &notification.app,
                    icon: "phone-symbolic",
                    summary,
                    body: &notification.text,
                    actions: Vec::new(),
                    urgent: false,
                },
                replaces,
            )
            .await;
        if let Some(popup) = popup
            && let Some(device) = self.lock().devices.get_mut(id) {
                device.popups.insert(nid, popup);
            }
    }

    pub fn reply_notification(&self, id: &str, reply_id: &str, message: &str) -> Reply {
        self.send_paired(
            id,
            Packet::new(
                "kdeconnect.notification.reply",
                json!({ "requestReplyId": reply_id, "message": message }),
            ),
        )
    }

    pub fn dismiss_notification(&self, id: &str, notification_id: &str) -> Reply {
        self.send_paired(
            id,
            Packet::new("kdeconnect.notification.request", json!({ "cancel": notification_id })),
        )?;
        if let Some(device) = self.lock().devices.get_mut(id) {
            device.notifications.remove(notification_id);
        }
        self.emit(Signal::Changed);
        Ok(())
    }

    // ---- Simple actions ----------------------------------------------------

    pub fn ring(&self, id: &str) -> Reply {
        self.send_paired(id, Packet::new("kdeconnect.findmyphone.request", json!({})))
    }

    pub fn ping(&self, id: &str) -> Reply {
        self.send_paired(id, Packet::new("kdeconnect.ping", json!({})))
    }

    pub fn send_clipboard(&self, id: &str, text: &str) -> Reply {
        self.send_paired(id, Packet::new("kdeconnect.clipboard", json!({ "content": text })))
    }

    /// Links open in the phone's browser; anything else lands on its clipboard.
    pub fn send_text(&self, id: &str, text: &str) -> Reply {
        let body = if looks_like_url(text) {
            json!({ "url": text.trim() })
        } else {
            json!({ "text": text })
        };
        self.send_paired(id, Packet::new("kdeconnect.share.request", body))
    }

    pub async fn browse(&self, id: &str) -> Reply {
        let mounted = self.lock().mounts.get(id).cloned();
        if let Some(mount) = mounted {
            let _ = open::that_detached(&mount);
            return Ok(());
        }
        self.send_paired(id, Packet::new("kdeconnect.sftp.request", json!({ "startBrowsing": true })))
    }

    async fn on_sftp(&self, id: &str, packet: &Packet) {
        if let Some(err) = packet.get_str("errorMessage") {
            self.simple_popup("Can't browse the phone", err).await;
            return;
        }
        let (Some(ip), Some(port), Some(user), Some(password)) = (
            packet.get_str("ip"),
            packet.get_i64("port"),
            packet.get_str("user"),
            packet.get_str("password"),
        ) else {
            return;
        };
        let path = packet.get_str("path").unwrap_or("/");
        let name = self
            .lock()
            .devices
            .get(id)
            .map(|d| d.info.name.clone())
            .unwrap_or_else(|| id.to_owned());
        let mount = dirs::runtime_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join("pocketlink")
            .join(sanitize_file_name(&name));

        match mount_sshfs(&mount, user, ip, port, path, password).await {
            Ok(()) => {
                self.lock().mounts.insert(id.to_owned(), mount.clone());
                let _ = open::that_detached(&mount);
            }
            Err(err) => {
                tracing::warn!(%err, "sshfs failed");
                self.simple_popup("Can't browse the phone", &err).await;
            }
        }
    }

    pub async fn unmount_all(&self) {
        let mounts: Vec<PathBuf> = self.lock().mounts.drain().map(|(_, m)| m).collect();
        for mount in mounts {
            unmount(&mount).await;
        }
    }

    // ---- Sharing -----------------------------------------------------------

    async fn on_share(self: &Arc<Self>, id: &str, name: &str, packet: Packet) {
        if let Some(url) = packet.get_str("url") {
            if url.starts_with("http://") || url.starts_with("https://") {
                let _ = open::that_detached(url);
            }
        } else if let Some(text) = packet.get_str("text") {
            self.emit(Signal::Clipboard {
                id: id.to_owned(),
                text: text.to_owned(),
            });
            self.simple_popup(&format!("Text from {name}"), text).await;
        } else if packet.payload_size.is_some()
            && let Err(err) = self.receive_file(id, packet) {
                tracing::warn!(%id, %err, "couldn't receive file");
            }
    }

    fn receive_file(self: &Arc<Self>, id: &str, packet: Packet) -> Reply {
        let (peer, cert, device_name) = {
            let st = self.lock();
            let device = st.devices.get(id).ok_or("Unknown device")?;
            let link = device.link.as_ref().ok_or("Device went offline")?;
            let cert = device.cert.clone().ok_or("No certificate")?;
            (link.peer, cert, device.info.name.clone())
        };
        let port = packet.payload_port().ok_or("No payload port")?;
        let size = packet.payload_size.unwrap_or_default().max(0) as u64;
        let name = sanitize_file_name(packet.get_str("filename").unwrap_or("file"));
        std::fs::create_dir_all(&self.downloads).map_err(|e| e.to_string())?;
        let path = unique_path(&self.downloads, &name);
        let part = path.with_file_name(format!(".{}.part", file_name(&path)));
        let tid = self.start_transfer(id, Direction::Incoming, &file_name(&path), size);

        let daemon = self.clone();
        let task = tokio::spawn(async move {
            let result = async {
                let file = tokio::fs::File::create(&part).await?;
                let mut sink = BufWriter::new(file);
                payload::receive(
                    &daemon.tls,
                    SocketAddr::new(peer.ip(), port),
                    &cert,
                    &mut sink,
                    size,
                    |n| daemon.progress(tid, n),
                )
                .await?;
                sink.into_inner().sync_all().await?;
                tokio::fs::rename(&part, &path).await?;
                Ok::<_, pocketlink_proto::Error>(())
            }
            .await;

            match result {
                Ok(()) => {
                    daemon.finish_transfer(tid, TransferState::Done, Some(&path));
                    let summary = format!("Received {}", file_name(&path));
                    let body = format!("From {device_name}");
                    daemon
                        .popup(
                            Note {
                                app: "Pocketlink",
                                icon: "document-save-symbolic",
                                summary: &summary,
                                body: &body,
                                actions: vec![("Open", Action::Open(path.clone()))],
                                urgent: false,
                            },
                            0,
                        )
                        .await;
                }
                Err(err) => {
                    let _ = tokio::fs::remove_file(&part).await;
                    daemon.finish_transfer(tid, TransferState::Failed { error: err.to_string() }, None);
                }
            }
        });
        self.set_abort(tid, task.abort_handle());
        Ok(())
    }

    pub fn send_files(self: &Arc<Self>, id: &str, paths: Vec<PathBuf>) -> Reply {
        let files: Vec<(PathBuf, u64, Option<i64>)> = paths
            .into_iter()
            .filter_map(|p| {
                let meta = std::fs::metadata(&p).ok().filter(|m| m.is_file())?;
                let modified = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_millis() as i64);
                Some((p, meta.len(), modified))
            })
            .collect();
        if files.is_empty() {
            return Err("There are no files to send (folders aren't supported yet)".into());
        }

        let (peer, cert, tx) = {
            let st = self.lock();
            let device = st.devices.get(id).ok_or("Unknown device")?;
            if device.pair != Pair::Paired {
                return Err(format!("{} isn't paired", device.info.name));
            }
            let link = device
                .link
                .as_ref()
                .ok_or_else(|| format!("{} is offline", device.info.name))?;
            (link.peer, device.cert.clone().ok_or("No certificate")?, link.tx.clone())
        };

        let total: u64 = files.iter().map(|(_, size, _)| size).sum();
        let label = match files.len() {
            1 => file_name(&files[0].0),
            n => format!("{n} files"),
        };
        let tid = self.start_transfer(id, Direction::Outgoing, &label, total);
        let count = files.len();

        let daemon = self.clone();
        let task = tokio::spawn(async move {
            let result = async {
                let mut done = 0u64;
                for (path, size, modified) in files {
                    let mut packet = Packet::new(
                        "kdeconnect.share.request",
                        json!({
                            "filename": file_name(&path),
                            "numberOfFiles": count,
                            "totalPayloadSize": total,
                            "open": false,
                        }),
                    );
                    if let Some(modified) = modified {
                        packet.set("lastModified", modified);
                    }
                    if size == 0 {
                        tx.send(packet).map_err(|_| disconnected())?;
                        continue;
                    }

                    let server = PayloadServer::bind(IpAddr::V4(Ipv4Addr::UNSPECIFIED)).await?;
                    packet.payload_size = Some(size as i64);
                    let mut info = serde_json::Map::new();
                    info.insert("port".into(), server.port().into());
                    packet.payload_transfer_info = Some(info);
                    tx.send(packet).map_err(|_| disconnected())?;

                    let file = tokio::fs::File::open(&path).await?;
                    let base = done;
                    server
                        .send(&daemon.tls, peer.ip(), &cert, file, size, |n| {
                            daemon.progress(tid, base + n)
                        })
                        .await?;
                    done += size;
                }
                Ok::<_, pocketlink_proto::Error>(())
            }
            .await;

            match result {
                Ok(()) => daemon.finish_transfer(tid, TransferState::Done, None),
                Err(err) => daemon.finish_transfer(tid, TransferState::Failed { error: err.to_string() }, None),
            }
        });
        self.set_abort(tid, task.abort_handle());
        Ok(())
    }

    fn start_transfer(&self, device_id: &str, direction: Direction, name: &str, total: u64) -> u64 {
        let tid = self.next();
        let info = api::Transfer {
            id: tid,
            device_id: device_id.to_owned(),
            direction,
            name: name.to_owned(),
            done: 0,
            total,
            state: TransferState::Active,
            path: None,
        };
        let mut st = self.lock();
        st.transfers.insert(
            tid,
            TransferEntry {
                info: info.clone(),
                abort: None,
                last_emit: Instant::now(),
            },
        );
        // Keep the last 100.
        while st.transfers.len() > 100 {
            st.transfers.pop_first();
        }
        drop(st);
        self.emit(Signal::Transfer(info));
        tid
    }

    fn set_abort(&self, tid: u64, abort: tokio::task::AbortHandle) {
        if let Some(entry) = self.lock().transfers.get_mut(&tid) {
            entry.abort = Some(abort);
        }
    }

    fn progress(&self, tid: u64, done: u64) {
        let info = {
            let mut st = self.lock();
            let Some(entry) = st.transfers.get_mut(&tid) else { return };
            entry.info.done = done;
            if entry.last_emit.elapsed() < PROGRESS_INTERVAL && done < entry.info.total {
                return;
            }
            entry.last_emit = Instant::now();
            entry.info.clone()
        };
        self.emit(Signal::Transfer(info));
    }

    fn finish_transfer(&self, tid: u64, state: TransferState, path: Option<&Path>) {
        let info = {
            let mut st = self.lock();
            let Some(entry) = st.transfers.get_mut(&tid) else { return };
            if entry.info.state != TransferState::Active {
                return;
            }
            if state == TransferState::Done {
                entry.info.done = entry.info.total;
            }
            entry.info.state = state;
            entry.info.path = path.map(|p| p.to_string_lossy().into_owned());
            entry.abort = None;
            entry.info.clone()
        };
        tracing::info!(name = %info.name, state = ?info.state, "transfer finished");
        self.emit(Signal::Transfer(info));
    }

    pub fn cancel_transfer(&self, tid: u64) -> Reply {
        let abort = self
            .lock()
            .transfers
            .get_mut(&tid)
            .and_then(|e| e.abort.take())
            .ok_or("That transfer isn't running")?;
        abort.abort();
        self.finish_transfer(tid, TransferState::Cancelled, None);
        Ok(())
    }

    // ---- Desktop notifications ----------------------------------------------

    pub fn on_action(self: &Arc<Self>, action: Action) {
        let result = match &action {
            Action::AcceptPair(id) => self.accept_pair(id),
            Action::RejectPair(id) => self.reject_pair(id),
            Action::Open(path) => open::that_detached(path).map_err(|e| e.to_string()),
        };
        if let Err(err) = result {
            tracing::warn!(?action, %err, "notification action failed");
        }
    }

    async fn popup(&self, note: Note<'_>, replaces: u32) -> Option<u32> {
        self.notifier.get()?.show(note, replaces).await
    }

    async fn simple_popup(&self, summary: &str, body: &str) {
        self.popup(
            Note {
                app: "Pocketlink",
                icon: "phone-symbolic",
                summary,
                body,
                actions: Vec::new(),
                urgent: false,
            },
            0,
        )
        .await;
    }

    async fn close_popup(&self, id: u32) {
        if let Some(notifier) = self.notifier.get() {
            notifier.close(id).await;
        }
    }

    fn after_popup_closed(self: &Arc<Self>, popup: Option<u32>) {
        if let Some(popup) = popup {
            let daemon = self.clone();
            tokio::spawn(async move { daemon.close_popup(popup).await });
        }
    }
}

async fn write_loop(mut writer: WriteHalf<lan::Stream>, mut rx: mpsc::UnboundedReceiver<Packet>) {
    while let Some(packet) = rx.recv().await {
        let result = async {
            writer.write_all(&packet.to_line()).await?;
            writer.flush().await
        }
        .await;
        if let Err(err) = result {
            tracing::debug!(%err, "link write failed");
            break;
        }
    }
    let _ = writer.shutdown().await;
}

async fn mount_sshfs(mount: &Path, user: &str, ip: &str, port: i64, path: &str, password: &str) -> Result<(), String> {
    use tokio::process::Command;

    tokio::fs::create_dir_all(mount).await.map_err(|e| e.to_string())?;
    let mut child = Command::new("sshfs")
        .arg(format!("{user}@{ip}:{path}"))
        .arg(mount)
        .args(["-p", &port.to_string()])
        .args(["-o", "password_stdin"])
        .args(["-o", "StrictHostKeyChecking=no"])
        .args(["-o", "UserKnownHostsFile=/dev/null"])
        .args(["-o", "HostKeyAlgorithms=+ssh-rsa"])
        .args(["-o", "reconnect,ServerAliveInterval=30,ServerAliveCountMax=3"])
        .stdin(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("Couldn't run sshfs: {e}. Install it with “sudo apt install sshfs”."))?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(format!("{password}\n").as_bytes()).await;
    }
    let output = tokio::time::timeout(Duration::from_secs(20), child.wait_with_output())
        .await
        .map_err(|_| "Mounting the phone took too long".to_owned())?
        .map_err(|e| e.to_string())?;
    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_owned())
    }
}

async fn unmount(mount: &Path) {
    for tool in ["fusermount3", "fusermount"] {
        let status = tokio::process::Command::new(tool)
            .args(["-u", "-z"])
            .arg(mount)
            .status()
            .await;
        if status.is_ok_and(|s| s.success()) {
            return;
        }
    }
}

fn disconnected() -> pocketlink_proto::Error {
    pocketlink_proto::Error::Protocol("device disconnected")
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

fn looks_like_url(text: &str) -> bool {
    let text = text.trim();
    !text.contains(char::is_whitespace) && (text.starts_with("https://") || text.starts_with("http://"))
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Only the last path component, and never something that escapes the folder.
fn sanitize_file_name(name: &str) -> String {
    let name = name.rsplit(['/', '\\']).next().unwrap_or_default();
    let name: String = name.chars().filter(|c| !c.is_control()).take(200).collect();
    let name = name.trim().trim_start_matches('.');
    if name.is_empty() { "file".to_owned() } else { name.to_owned() }
}

/// `photo.jpg`, or `photo (1).jpg` if that's taken.
fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let candidate = dir.join(name);
    if !candidate.exists() {
        return candidate;
    }
    let path = Path::new(name);
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let ext = path.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    (1..)
        .map(|n| dir.join(format!("{stem} ({n}){ext}")))
        .find(|p| !p.exists())
        .expect("some name is free")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_names_stay_in_the_folder() {
        assert_eq!(sanitize_file_name("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_file_name("..\\x.txt"), "x.txt");
        assert_eq!(sanitize_file_name(".."), "file");
        assert_eq!(sanitize_file_name(".bashrc"), "bashrc");
        assert_eq!(sanitize_file_name("IMG 1.jpg"), "IMG 1.jpg");
        assert_eq!(sanitize_file_name(""), "file");
    }

    #[test]
    fn unique_names() {
        let dir = std::env::temp_dir().join(format!("pocketlink-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "").unwrap();
        std::fs::write(dir.join("a (1).txt"), "").unwrap();
        assert_eq!(unique_path(&dir, "a.txt"), dir.join("a (2).txt"));
        assert_eq!(unique_path(&dir, "b"), dir.join("b"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn urls() {
        assert!(looks_like_url(" https://example.com/x "));
        assert!(!looks_like_url("see https://example.com"));
        assert!(!looks_like_url("file:///etc/passwd"));
    }
}
