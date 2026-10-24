// SPDX-License-Identifier: GPL-3.0-or-later

//! Two devices on loopback find each other, connect, talk, and move a file.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use pocketlink_proto::lan::{self, Lan, LanConfig};
use pocketlink_proto::payload::{self, PayloadServer};
use pocketlink_proto::{Connection, DeviceInfo, Identity, Packet, PROTOCOL_VERSION, TlsConfigs};
use serde_json::json;
use tokio::io::AsyncWriteExt;
use tokio::sync::{mpsc, watch};
use tokio::time::timeout;

const LOCALHOST: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

struct Node {
    lan: Lan,
    tls: TlsConfigs,
    identity: Identity,
    connections: mpsc::Receiver<Connection>,
}

async fn node(id: &str, name: &str) -> Node {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(std::env::var("RUST_LOG").unwrap_or_default())
        .with_test_writer()
        .try_init();
    let identity = Identity::generate(id).unwrap();
    let tls = TlsConfigs::new(&identity).unwrap();
    let info = DeviceInfo {
        id: id.into(),
        name: name.into(),
        kind: "desktop".into(),
        protocol_version: PROTOCOL_VERSION,
        incoming: vec!["kdeconnect.ping".into()],
        outgoing: vec!["kdeconnect.ping".into()],
    };
    let (_tx, me) = watch::channel(info);
    std::mem::forget(_tx);
    let (tx, connections) = mpsc::channel(4);
    let config = LanConfig {
        udp_port: 0,
        tcp_ports: 0..=0,
        bind: LOCALHOST,
        loopback: true,
        ..LanConfig::default()
    };
    let lan = Lan::start(config, me, tls.clone(), tx).await.unwrap();
    Node {
        lan,
        tls,
        identity,
        connections,
    }
}

async fn next(node: &mut Node) -> Connection {
    timeout(Duration::from_secs(10), node.connections.recv())
        .await
        .expect("no connection in time")
        .expect("channel open")
}

#[tokio::test]
async fn discover_talk_and_transfer() {
    let mut a = node("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "Alpha").await;
    let mut b = node("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb", "Beta").await;

    // B announces itself to A; A connects back to B.
    b.lan
        .announce_to(SocketAddr::new(LOCALHOST, a.lan.udp_port()))
        .await;

    let mut at_a = next(&mut a).await;
    let mut at_b = next(&mut b).await;
    assert_eq!(at_a.info.name, "Beta");
    assert_eq!(at_b.info.name, "Alpha");
    assert_eq!(at_a.cert, b.identity.cert);
    assert_eq!(at_b.cert, a.identity.cert);

    // Packets both ways.
    let ping = Packet::new("kdeconnect.ping", json!({ "message": "hello" }));
    at_a.writer.write_all(&ping.to_line()).await.unwrap();
    let got = lan::read_packet(&mut at_b.reader).await.unwrap().unwrap();
    assert_eq!(got.get_str("message"), Some("hello"));

    let pong = Packet::new("kdeconnect.ping", json!({ "message": "hi back" }));
    at_b.writer.write_all(&pong.to_line()).await.unwrap();
    let got = lan::read_packet(&mut at_a.reader).await.unwrap().unwrap();
    assert_eq!(got.get_str("message"), Some("hi back"));

    // A sends B a 1 MB file.
    let data: Vec<u8> = (0..1_000_000u32).map(|i| (i % 251) as u8).collect();
    let server = PayloadServer::bind_in(LOCALHOST, 0..=0).await.unwrap();
    let port = server.port();
    let size = data.len() as u64;

    let sender = {
        let tls = a.tls.clone();
        let expected = b.identity.cert.clone();
        let data = data.clone();
        tokio::spawn(async move {
            server
                .send(&tls, LOCALHOST, &expected, &data[..], size, |_| {})
                .await
        })
    };

    let mut received = Vec::new();
    let mut last = 0;
    payload::receive(
        &b.tls,
        SocketAddr::new(LOCALHOST, port),
        &a.identity.cert,
        &mut received,
        size,
        |n| last = n,
    )
    .await
    .unwrap();
    sender.await.unwrap().unwrap();
    assert_eq!(last, size);
    assert_eq!(received, data);
}

#[tokio::test]
async fn payload_rejects_wrong_device() {
    let a = node("cccccccccccccccccccccccccccccccc", "Gamma").await;
    let b = node("dddddddddddddddddddddddddddddddd", "Delta").await;
    let stranger = Identity::generate("eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee").unwrap();

    let server = PayloadServer::bind_in(LOCALHOST, 0..=0).await.unwrap();
    let port = server.port();
    let tls = a.tls.clone();
    // A only wants to give the file to `stranger`, but B connects.
    let sender = tokio::spawn(async move {
        server
            .send(&tls, LOCALHOST, &stranger.cert, &b"secret"[..], 6, |_| {})
            .await
    });
    let mut sink = Vec::new();
    let _ = payload::receive(
        &b.tls,
        SocketAddr::new(LOCALHOST, port),
        &a.identity.cert,
        &mut sink,
        6,
        |_| {},
    )
    .await;
    assert!(matches!(
        sender.await.unwrap(),
        Err(pocketlink_proto::Error::Untrusted)
    ));
    assert!(sink.is_empty());
}
