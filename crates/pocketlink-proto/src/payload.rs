// SPDX-License-Identifier: GPL-3.0-or-later

//! File contents travel on their own TLS connection.
//!
//! The sender listens on a port in 1739–1764 and puts it in the packet's
//! `payloadTransferInfo`. The receiver connects and is the TLS client. Both
//! sides check that the other end is the same paired device.

use std::io;
use std::net::{IpAddr, SocketAddr};
use std::ops::RangeInclusive;
use std::time::Duration;

use rustls_pki_types::{CertificateDer, ServerName};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;
use tokio_rustls::{TlsAcceptor, TlsConnector};

use crate::tls::TlsConfigs;
use crate::{Error, Result};

pub const PAYLOAD_PORTS: RangeInclusive<u16> = 1739..=1764;

const ACCEPT_TIMEOUT: Duration = Duration::from_secs(30);
const CHUNK: usize = 64 * 1024;

/// A listening socket waiting for the peer to fetch one payload.
pub struct PayloadServer {
    listener: TcpListener,
    port: u16,
}

impl PayloadServer {
    pub async fn bind(ip: IpAddr) -> io::Result<Self> {
        Self::bind_in(ip, PAYLOAD_PORTS).await
    }

    pub async fn bind_in(ip: IpAddr, ports: RangeInclusive<u16>) -> io::Result<Self> {
        let mut last_err = None;
        for port in ports {
            match TcpListener::bind(SocketAddr::new(ip, port)).await {
                Ok(listener) => {
                    let port = listener.local_addr()?.port();
                    return Ok(Self { listener, port });
                }
                Err(err) => last_err = Some(err),
            }
        }
        Err(last_err.unwrap_or_else(|| io::Error::new(io::ErrorKind::AddrInUse, "no free port")))
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// Wait for `peer` to connect, then stream `size` bytes from `source`.
    pub async fn send<R: AsyncRead + Unpin>(
        self,
        tls: &TlsConfigs,
        peer: IpAddr,
        expected: &CertificateDer<'static>,
        mut source: R,
        size: u64,
        mut progress: impl FnMut(u64),
    ) -> Result<()> {
        let stream = loop {
            let (stream, from) = timeout(ACCEPT_TIMEOUT, self.listener.accept())
                .await
                .map_err(|_| Error::Timeout)??;
            if same_host(from.ip(), peer) {
                break stream;
            }
            tracing::warn!(%from, "ignoring payload connection from the wrong address");
        };

        let mut tls = TlsAcceptor::from(tls.server.clone()).accept(stream).await?;
        let cert = tls.get_ref().1.peer_certificates().and_then(|c| c.first());
        if cert != Some(expected) {
            return Err(Error::Untrusted);
        }

        let mut buf = vec![0u8; CHUNK];
        let mut sent = 0u64;
        while sent < size {
            let want = CHUNK.min((size - sent) as usize);
            let n = source.read(&mut buf[..want]).await?;
            if n == 0 {
                return Err(Error::Protocol("file shrank while sending"));
            }
            tls.write_all(&buf[..n]).await?;
            sent += n as u64;
            progress(sent);
        }
        tls.flush().await?;
        let _ = tls.shutdown().await;
        Ok(())
    }
}

/// Fetch `size` bytes from the peer's payload port into `sink`.
pub async fn receive<W: AsyncWrite + Unpin>(
    tls: &TlsConfigs,
    addr: SocketAddr,
    expected: &CertificateDer<'static>,
    mut sink: W,
    size: u64,
    mut progress: impl FnMut(u64),
) -> Result<()> {
    let stream = timeout(ACCEPT_TIMEOUT, TcpStream::connect(addr))
        .await
        .map_err(|_| Error::Timeout)??;
    let mut tls = TlsConnector::from(tls.client.clone())
        .connect(ServerName::from(addr.ip()), stream)
        .await?;
    let cert = tls.get_ref().1.peer_certificates().and_then(|c| c.first());
    if cert != Some(expected) {
        return Err(Error::Untrusted);
    }

    let mut buf = vec![0u8; CHUNK];
    let mut received = 0u64;
    while received < size {
        let want = CHUNK.min((size - received) as usize);
        let n = match tls.read(&mut buf[..want]).await {
            Ok(0) => return Err(Error::Protocol("payload ended early")),
            Ok(n) => n,
            // Some senders close without a TLS close_notify once everything is written.
            Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => {
                return Err(Error::Protocol("payload ended early"));
            }
            Err(err) => return Err(err.into()),
        };
        sink.write_all(&buf[..n]).await?;
        received += n as u64;
        progress(received);
    }
    sink.flush().await?;
    Ok(())
}

fn same_host(a: IpAddr, b: IpAddr) -> bool {
    a.to_canonical() == b.to_canonical()
}
