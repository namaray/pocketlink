// SPDX-License-Identifier: GPL-3.0-or-later

//! Runs the real daemon and talks to it as a fake phone, over the real network
//! stack and the real D-Bus session bus.
//!
//! Needs a session bus and a LAN address, so it's ignored by default:
//! `cargo test -p pocketlink-daemon --test e2e -- --ignored`

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::Duration;

use pocketlink_core::{Client, Device, PairState, TransferState};
use pocketlink_proto::lan::{self, Lan, LanConfig};
use pocketlink_proto::payload::{self, PayloadServer};
use pocketlink_proto::{Connection, DeviceInfo, Identity, Packet, PROTOCOL_VERSION, TlsConfigs};
use serde_json::json;
use tokio::io::AsyncWriteExt;
use tokio::sync::{mpsc, watch};
use tokio::time::{sleep, timeout};

const PHONE_ID: &str = "e2e0phone0000000000000000000000a";
const UDP_PORT: u16 = 17161;

struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pocketlink-e2e-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn lan_ip() -> IpAddr {
    if_addrs::get_if_addrs()
        .unwrap()
        .into_iter()
        .find(|i| !i.is_loopback() && i.ip().is_ipv4())
        .expect("a LAN address")
        .ip()
}

async fn wait_for<T>(what: &str, mut check: impl AsyncFnMut() -> Option<T>) -> T {
    for _ in 0..100 {
        if let Some(value) = check().await {
            return value;
        }
        sleep(Duration::from_millis(100)).await;
    }
    panic!("timed out waiting for {what}");
}

async fn device(client: &Client) -> Option<Device> {
    client
        .devices()
        .await
        .ok()?
        .into_iter()
        .find(|d| d.id == PHONE_ID)
}

async fn expect(packets: &mut mpsc::UnboundedReceiver<Packet>, kind: &str) -> Packet {
    loop {
        let packet = timeout(Duration::from_secs(10), packets.recv())
            .await
            .unwrap_or_else(|_| panic!("no {kind} packet"))
            .expect("link open");
        if packet.kind == kind {
            return packet;
        }
    }
}

fn file_bytes(len: usize, seed: u8) -> Vec<u8> {
    (0..len).map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed)).collect()
}

fn read(path: &Path) -> Option<Vec<u8>> {
    std::fs::read(path).ok()
}

#[tokio::test]
#[ignore = "needs a D-Bus session bus and a LAN address"]
async fn fake_phone_pairs_and_shares() {
    let config_dir = temp_dir("config");
    let downloads = temp_dir("downloads");
    let _daemon = Daemon(
        Command::new(env!("CARGO_BIN_EXE_pocketlink-daemon"))
            .env("POCKETLINK_CONFIG_DIR", &config_dir)
            .env("POCKETLINK_DOWNLOAD_DIR", &downloads)
            .env("POCKETLINK_UDP_PORT", UDP_PORT.to_string())
            .env("POCKETLINK_NO_NOTIFY", "1")
            .env("POCKETLINK_NO_BROADCAST", "1")
            .env("RUST_LOG", "info,pocketlink_proto=debug")
            .spawn()
            .expect("daemon starts"),
    );

    let client = wait_for("the daemon on D-Bus", async || {
        let client = Client::connect().await.ok()?;
        client.devices().await.ok()?;
        Some(client)
    })
    .await;

    // --- The fake phone announces itself; the daemon connects back. ---
    let identity = Identity::generate(PHONE_ID).unwrap();
    let tls = TlsConfigs::new(&identity).unwrap();
    let info = DeviceInfo {
        id: PHONE_ID.into(),
        name: "E2E Phone".into(),
        kind: "phone".into(),
        protocol_version: PROTOCOL_VERSION,
        incoming: [
            "kdeconnect.ping",
            "kdeconnect.findmyphone.request",
            "kdeconnect.share.request",
            "kdeconnect.clipboard",
            "kdeconnect.notification.request",
            "kdeconnect.notification.reply",
        ]
        .map(String::from)
        .to_vec(),
        outgoing: ["kdeconnect.battery", "kdeconnect.notification", "kdeconnect.share.request"]
            .map(String::from)
            .to_vec(),
    };
    let (_me_tx, me_rx) = watch::channel(info);
    let (conn_tx, mut conn_rx) = mpsc::channel(4);
    let config = LanConfig {
        udp_port: 0,
        announce_port: UDP_PORT,
        ..LanConfig::default()
    };
    let phone = Lan::start(config, me_rx, tls.clone(), conn_tx).await.unwrap();
    let daemon_ip = lan_ip();
    phone.announce_to(SocketAddr::new(daemon_ip, UDP_PORT)).await;

    let Connection {
        cert: daemon_cert,
        mut reader,
        mut writer,
        peer,
        ..
    } = timeout(Duration::from_secs(10), conn_rx.recv())
        .await
        .expect("daemon connected to the phone")
        .unwrap();

    let (packets_tx, mut packets) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Ok(Some(packet)) = lan::read_packet(&mut reader).await {
            let _ = packets_tx.send(packet);
        }
    });
    let send = async |writer: &mut tokio::io::WriteHalf<lan::Stream>, packet: Packet| {
        writer.write_all(&packet.to_line()).await.unwrap();
        writer.flush().await.unwrap();
    };

    let seen = wait_for("the phone in the device list", async || device(&client).await).await;
    assert!(seen.reachable);
    assert_eq!(seen.pair, PairState::Unpaired);

    // --- Pairing: the phone asks, the PC accepts. ---
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    send(&mut writer, Packet::new("kdeconnect.pair", json!({ "pair": true, "timestamp": now }))).await;

    let code = wait_for("the pairing request", async || match device(&client).await?.pair {
        PairState::RequestedByPeer { code } => Some(code),
        _ => None,
    })
    .await;
    assert_eq!(
        Some(code),
        pocketlink_proto::identity::verification_key(&identity.cert, &daemon_cert, now as i64)
    );
    client.accept_pair(PHONE_ID).await.unwrap();
    assert_eq!(expect(&mut packets, "kdeconnect.pair").await.get_bool("pair"), Some(true));
    expect(&mut packets, "kdeconnect.notification.request").await;
    assert!(device(&client).await.unwrap().paired());
    assert!(std::fs::read_to_string(config_dir.join("trusted.json")).unwrap().contains(PHONE_ID));

    // --- Battery and notifications show up. ---
    send(
        &mut writer,
        Packet::new("kdeconnect.battery", json!({ "currentCharge": 42, "isCharging": true })),
    )
    .await;
    send(
        &mut writer,
        Packet::new(
            "kdeconnect.notification",
            json!({
                "id": "n1", "appName": "Messages", "title": "Sam", "text": "Lunch?",
                "isClearable": true, "requestReplyId": "r1", "silent": true,
            }),
        ),
    )
    .await;
    let seen = wait_for("battery and notification", async || {
        let d = device(&client).await?;
        (d.battery.is_some() && !d.notifications.is_empty()).then_some(d)
    })
    .await;
    assert_eq!(seen.battery.unwrap().charge, 42);
    assert!(seen.battery.unwrap().charging);
    assert_eq!(seen.notifications[0].text, "Lunch?");
    assert_eq!(seen.notifications[0].reply_id.as_deref(), Some("r1"));

    // --- Replying, ringing. ---
    client.reply_notification(PHONE_ID, "r1", "Sure!").await.unwrap();
    let reply = expect(&mut packets, "kdeconnect.notification.reply").await;
    assert_eq!(reply.get_str("message"), Some("Sure!"));
    client.ring(PHONE_ID).await.unwrap();
    expect(&mut packets, "kdeconnect.findmyphone.request").await;

    // --- PC → phone file. ---
    let outgoing = config_dir.join("holiday.jpg");
    let outgoing_bytes = file_bytes(300_000, 7);
    std::fs::write(&outgoing, &outgoing_bytes).unwrap();
    client.send_files(PHONE_ID, &[&outgoing]).await.unwrap();
    let offer = expect(&mut packets, "kdeconnect.share.request").await;
    assert_eq!(offer.get_str("filename"), Some("holiday.jpg"));
    assert_eq!(offer.payload_size, Some(300_000));
    let mut got = Vec::new();
    payload::receive(
        &tls,
        SocketAddr::new(peer.ip(), offer.payload_port().unwrap()),
        &daemon_cert,
        &mut got,
        300_000,
        |_| {},
    )
    .await
    .unwrap();
    assert_eq!(got, outgoing_bytes);
    wait_for("the outgoing transfer to finish", async || {
        let transfers = client.transfers().await.ok()?;
        transfers
            .iter()
            .any(|t| t.name == "holiday.jpg" && t.state == TransferState::Done)
            .then_some(())
    })
    .await;

    // --- Phone → PC file, with a hostile name. ---
    let incoming_bytes = file_bytes(123_456, 3);
    let server = PayloadServer::bind(IpAddr::V4(Ipv4Addr::UNSPECIFIED)).await.unwrap();
    let mut share = Packet::new("kdeconnect.share.request", json!({ "filename": "../../evil.txt" }));
    share.payload_size = Some(incoming_bytes.len() as i64);
    let mut transfer_info = serde_json::Map::new();
    transfer_info.insert("port".into(), server.port().into());
    share.payload_transfer_info = Some(transfer_info);
    send(&mut writer, share).await;
    server
        .send(&tls, peer.ip(), &daemon_cert, &incoming_bytes[..], incoming_bytes.len() as u64, |_| {})
        .await
        .unwrap();
    let saved = downloads.join("evil.txt");
    wait_for("the received file", async || {
        (read(&saved).as_deref() == Some(&incoming_bytes[..])).then_some(())
    })
    .await;
    assert!(!config_dir.parent().unwrap().join("evil.txt").exists());

    // --- Disconnecting keeps the paired device, offline. ---
    writer.shutdown().await.unwrap();
    drop(writer);
    let seen = wait_for("the phone to go offline", async || {
        let d = device(&client).await?;
        (!d.reachable).then_some(d)
    })
    .await;
    assert!(seen.paired());

    let _ = std::fs::remove_dir_all(&config_dir);
    let _ = std::fs::remove_dir_all(&downloads);
}
