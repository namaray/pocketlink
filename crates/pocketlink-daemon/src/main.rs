// SPDX-License-Identifier: GPL-3.0-or-later

//! `pocketlink-daemon`: keeps your phone connected in the background.

mod daemon;
mod dbus;
mod notify;
mod store;

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use pocketlink_core::{BUS_NAME, OBJECT_PATH};
use pocketlink_proto::{DeviceInfo, Lan, LanConfig, PROTOCOL_VERSION, TlsConfigs};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::{mpsc, watch};

use crate::daemon::{Daemon, INCOMING, OUTGOING};

const BROADCAST_EVERY: Duration = Duration::from_secs(60);

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,zbus=warn".into()),
        )
        .init();

    if let Err(err) = run().await {
        tracing::error!("{err}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let store = store::Store::open()?;
    let identity = store.identity()?;
    let tls = TlsConfigs::new(&identity)?;
    let settings = store.settings();

    let me = DeviceInfo {
        id: identity.device_id.clone(),
        name: pocketlink_proto::identity::filter_name(
            &settings.name.clone().unwrap_or_else(store::hostname),
        ),
        kind: store::device_kind().to_owned(),
        protocol_version: PROTOCOL_VERSION,
        incoming: INCOMING.iter().map(|s| (*s).to_owned()).collect(),
        outgoing: OUTGOING.iter().map(|s| (*s).to_owned()).collect(),
    };
    tracing::info!(id = %me.id, name = %me.name, "starting");
    let (me_tx, me_rx) = watch::channel(me);

    let (signals_tx, signals_rx) = mpsc::unbounded_channel();
    let daemon = Arc::new(Daemon::new(store, identity, tls.clone(), me_tx, signals_tx));

    let bus = zbus::connection::Builder::session()?
        .serve_at(OBJECT_PATH, dbus::Api { daemon: daemon.clone() })?
        .build()
        .await?;
    // Claim the name last, so nobody calls us before we're ready.
    bus.request_name_with_flags(BUS_NAME, zbus::fdo::RequestNameFlags::DoNotQueue.into())
        .await
        .map_err(|_| "Pocketlink is already running")?;
    tokio::spawn(dbus::emit_signals(bus.clone(), signals_rx));

    if !test_mode("POCKETLINK_NO_NOTIFY") {
        let notifier = notify::Notifier::new(&bus).await;
        let actions = daemon.clone();
        notifier.listen(move |action| actions.on_action(action)).await;
        daemon.set_notifier(notifier);
    }

    let mut config = LanConfig {
        extra_targets: settings
            .addresses
            .iter()
            .filter_map(|a| a.parse::<IpAddr>().ok())
            .collect(),
        ..LanConfig::default()
    };
    if let Some(port) = std::env::var("POCKETLINK_UDP_PORT").ok().and_then(|p| p.parse().ok()) {
        config.udp_port = port;
    }
    let (conn_tx, conn_rx) = mpsc::channel(16);
    let lan = Lan::start(config, me_rx, tls, conn_tx).await?;
    daemon.set_lan(lan.clone());
    tokio::spawn(daemon.clone().run(conn_rx));

    if !test_mode("POCKETLINK_NO_BROADCAST") {
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(BROADCAST_EVERY);
            loop {
                tick.tick().await;
                lan.broadcast().await;
            }
        });
    }

    let mut term = signal(SignalKind::terminate())?;
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
    tracing::info!("shutting down");
    daemon.unmount_all().await;
    Ok(())
}

/// Switches for the end-to-end tests, so they don't reach real phones or pop up notifications.
fn test_mode(var: &str) -> bool {
    std::env::var_os(var).is_some_and(|v| v == "1")
}
