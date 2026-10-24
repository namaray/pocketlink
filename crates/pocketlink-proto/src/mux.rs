// SPDX-License-Identifier: GPL-3.0-or-later

//! The Bluetooth multiplexing protocol (version 1).
//!
//! Bluetooth gives us one RFCOMM stream per device, so packets and file
//! payloads share it as separate *channels*. Every frame is:
//!
//! ```text
//! | type: 1 byte | length: 2 bytes BE | channel UUID: 16 bytes | data: `length` bytes |
//! ```
//!
//! Flow control is credit based: a side may only write as many bytes on a
//! channel as the other side asked for with READ frames. Each side keeps at
//! most 4096 bytes requested, because the Android app can't take more per frame.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{Notify, mpsc};
use uuid::Uuid;

/// Carries packets. Exists from the start without being opened.
pub const DEFAULT_CHANNEL: Uuid = uuid::uuid!("a0d0aaf4-1072-4d81-aa35-902a954b1266");

const BUFFER_SIZE: usize = 4096;
/// One per file in flight; a peer opening more than this is misbehaving.
const MAX_CHANNELS: usize = 256;
const HEADER: usize = 19;

const VERSION: u8 = 0;
const OPEN: u8 = 1;
const CLOSE: u8 = 2;
const READ: u8 = 3;
const WRITE: u8 = 4;

/// One multiplexed connection. Cheap to clone.
#[derive(Clone)]
pub struct Mux {
    shared: Arc<Shared>,
}

struct Shared {
    out: mpsc::UnboundedSender<Frame>,
    channels: Mutex<HashMap<Uuid, Arc<ChannelShared>>>,
    open: Mutex<bool>,
}

enum Frame {
    Data(Vec<u8>),
    Shutdown,
}

struct ChannelShared {
    id: Uuid,
    state: Mutex<ChannelState>,
    notify: Notify,
}

#[derive(Default)]
struct ChannelState {
    /// Received, not yet read.
    buf: VecDeque<u8>,
    /// Bytes we asked the peer for and haven't received yet.
    requested: usize,
    /// Bytes the peer asked us for and we haven't sent yet.
    credit: usize,
    closed: bool,
}

impl ChannelShared {
    fn new(id: Uuid) -> Arc<Self> {
        Arc::new(Self {
            id,
            state: Mutex::new(ChannelState::default()),
            notify: Notify::new(),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ChannelState> {
        self.state.lock().expect("channel lock poisoned")
    }

    fn mark_closed(&self) {
        self.lock().closed = true;
        self.notify.notify_waiters();
    }
}

impl Mux {
    /// Start multiplexing over `stream`. Sends our version right away.
    pub fn new<S>(stream: S) -> Self
    where
        S: AsyncRead + AsyncWrite + Send + 'static,
    {
        let (reader, writer) = tokio::io::split(stream);
        let (out, rx) = mpsc::unbounded_channel();
        let mut channels = HashMap::new();
        channels.insert(DEFAULT_CHANNEL, ChannelShared::new(DEFAULT_CHANNEL));
        let mux = Mux {
            shared: Arc::new(Shared {
                out,
                channels: Mutex::new(channels),
                open: Mutex::new(true),
            }),
        };

        let mut version = frame(VERSION, Uuid::nil(), 4);
        version.extend_from_slice(&1u16.to_be_bytes());
        version.extend_from_slice(&1u16.to_be_bytes());
        mux.send(version);
        // Let the peer talk first without waiting for us to start reading.
        if let Some(default) = mux.take_channel(DEFAULT_CHANNEL) {
            default.top_up(&mut default.ch.lock());
        }

        tokio::spawn(write_loop(writer, rx, mux.clone()));
        tokio::spawn(mux.clone().read_loop(reader));
        mux
    }

    pub fn is_open(&self) -> bool {
        *self.shared.open.lock().expect("not poisoned")
    }

    pub fn default_channel(&self) -> Channel {
        self.take_channel(DEFAULT_CHANNEL)
            .unwrap_or_else(|| self.closed_channel(DEFAULT_CHANNEL))
    }

    /// A new channel for sending a payload. Tell the peer its id, then write to it.
    pub fn open_channel(&self) -> Channel {
        let id = Uuid::new_v4();
        let ch = ChannelShared::new(id);
        self.channels().insert(id, ch.clone());
        self.send(frame(OPEN, id, 0));
        Channel {
            mux: self.clone(),
            ch,
        }
    }

    /// A channel the peer opened, for receiving a payload.
    pub fn take_channel(&self, id: Uuid) -> Option<Channel> {
        let ch = self.channels().get(&id).cloned()?;
        Some(Channel {
            mux: self.clone(),
            ch,
        })
    }

    /// Close everything; the stream is shut down.
    pub fn shutdown(&self) {
        self.close_all();
        let _ = self.shared.out.send(Frame::Shutdown);
    }

    fn closed_channel(&self, id: Uuid) -> Channel {
        let ch = ChannelShared::new(id);
        ch.lock().closed = true;
        Channel {
            mux: self.clone(),
            ch,
        }
    }

    fn channels(&self) -> std::sync::MutexGuard<'_, HashMap<Uuid, Arc<ChannelShared>>> {
        self.shared.channels.lock().expect("channels lock poisoned")
    }

    fn send(&self, data: Vec<u8>) {
        let _ = self.shared.out.send(Frame::Data(data));
    }

    fn close_all(&self) {
        *self.shared.open.lock().expect("not poisoned") = false;
        let channels: Vec<_> = self.channels().drain().map(|(_, c)| c).collect();
        for ch in channels {
            ch.mark_closed();
        }
    }

    async fn read_loop<R: AsyncRead + Unpin>(self, mut reader: R) {
        if let Err(err) = self.read_frames(&mut reader).await {
            tracing::debug!(%err, "multiplexed connection ended");
        }
        self.shutdown();
    }

    async fn read_frames<R: AsyncRead + Unpin>(&self, reader: &mut R) -> io::Result<()> {
        let mut got_version = false;
        let mut header = [0u8; HEADER];
        let mut data = vec![0u8; u16::MAX as usize];
        loop {
            reader.read_exact(&mut header).await?;
            let kind = header[0];
            let len = u16::from_be_bytes([header[1], header[2]]) as usize;
            let id = Uuid::from_bytes(header[3..].try_into().expect("16 bytes"));
            let data = &mut data[..len];
            reader.read_exact(data).await?;

            if !got_version && kind != VERSION {
                return Err(invalid("peer didn't start with a version message"));
            }
            match kind {
                VERSION => {
                    if len < 4 {
                        return Err(invalid("short version message"));
                    }
                    let min = u16::from_be_bytes([data[0], data[1]]);
                    let max = u16::from_be_bytes([data[2], data[3]]);
                    if min > 1 || max < 1 {
                        return Err(invalid("no common multiplexing version"));
                    }
                    got_version = true;
                }
                OPEN => {
                    let mut channels = self.channels();
                    if channels.len() >= MAX_CHANNELS && !channels.contains_key(&id) {
                        return Err(invalid("too many open channels"));
                    }
                    channels.entry(id).or_insert_with(|| ChannelShared::new(id));
                }
                CLOSE => {
                    let ch = self.channels().remove(&id);
                    if let Some(ch) = ch {
                        ch.mark_closed();
                    }
                }
                READ => {
                    if len != 2 {
                        return Err(invalid("bad read request"));
                    }
                    let amount = u16::from_be_bytes([data[0], data[1]]) as usize;
                    let ch = self.channels().get(&id).cloned();
                    if let Some(ch) = ch {
                        ch.lock().credit += amount;
                        ch.notify.notify_waiters();
                    }
                }
                WRITE => {
                    let ch = self.channels().get(&id).cloned();
                    if let Some(ch) = ch {
                        let mut st = ch.lock();
                        if len > st.requested {
                            return Err(invalid("peer wrote more than we asked for"));
                        }
                        st.requested -= len;
                        st.buf.extend(data.iter());
                        drop(st);
                        ch.notify.notify_waiters();
                    }
                }
                _ => return Err(invalid("unknown multiplexing message")),
            }
        }
    }
}

async fn write_loop<W: AsyncWrite + Unpin>(mut writer: W, mut rx: mpsc::UnboundedReceiver<Frame>, mux: Mux) {
    let mut shutdown = false;
    while let Some(frame) = rx.recv().await {
        let Frame::Data(data) = frame else { break };
        let result = async {
            writer.write_all(&data).await?;
            // Batch whatever else is queued before flushing.
            loop {
                match rx.try_recv() {
                    Ok(Frame::Data(more)) => writer.write_all(&more).await?,
                    Ok(Frame::Shutdown) => {
                        shutdown = true;
                        break;
                    }
                    Err(_) => break,
                }
            }
            writer.flush().await
        }
        .await;
        if let Err(err) = result {
            tracing::debug!(%err, "multiplexed write failed");
            break;
        }
        if shutdown {
            break;
        }
    }
    mux.close_all();
    let _ = writer.shutdown().await;
}

/// One channel. Reads return 0 once the peer closed it and everything was read.
#[derive(Clone)]
pub struct Channel {
    mux: Mux,
    ch: Arc<ChannelShared>,
}

impl Channel {
    pub fn id(&self) -> Uuid {
        self.ch.id
    }

    pub async fn read(&self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        loop {
            let notified = self.ch.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let mut st = self.ch.lock();
                let n = out.len().min(st.buf.len());
                for (slot, byte) in out.iter_mut().zip(st.buf.drain(..n)) {
                    *slot = byte;
                }
                if !st.closed {
                    self.top_up(&mut st);
                }
                if n > 0 {
                    return Ok(n);
                }
                if st.closed {
                    return Ok(0);
                }
            }
            notified.await;
        }
    }

    /// Ask for enough data to keep up to a full buffer in flight.
    fn top_up(&self, st: &mut ChannelState) {
        let in_flight = st.requested + st.buf.len();
        if in_flight < BUFFER_SIZE {
            let amount = BUFFER_SIZE - in_flight;
            st.requested += amount;
            let mut request = frame(READ, self.ch.id, 2);
            request.extend_from_slice(&(amount as u16).to_be_bytes());
            self.mux.send(request);
        }
    }

    pub async fn read_exact(&self, out: &mut [u8]) -> io::Result<()> {
        let mut filled = 0;
        while filled < out.len() {
            let n = self.read(&mut out[filled..]).await?;
            if n == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            filled += n;
        }
        Ok(())
    }

    pub async fn write_all(&self, mut data: &[u8]) -> io::Result<()> {
        while !data.is_empty() {
            let notified = self.ch.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let mut st = self.ch.lock();
                if st.closed {
                    return Err(io::ErrorKind::BrokenPipe.into());
                }
                if st.credit > 0 {
                    let n = st.credit.min(data.len()).min(BUFFER_SIZE);
                    st.credit -= n;
                    let mut write = frame(WRITE, self.ch.id, n);
                    write.extend_from_slice(&data[..n]);
                    self.mux.send(write);
                    data = &data[n..];
                    continue;
                }
            }
            notified.await;
        }
        Ok(())
    }

    pub fn close(&self) {
        let was_open = {
            let mut st = self.ch.lock();
            !std::mem::replace(&mut st.closed, true)
        };
        self.ch.notify.notify_waiters();
        if was_open {
            self.mux.channels().remove(&self.ch.id);
            self.mux.send(frame(CLOSE, self.ch.id, 0));
        }
    }
}

/// Reads newline-terminated packets from a channel.
pub struct LineReader {
    channel: Channel,
    pending: Vec<u8>,
}

impl LineReader {
    pub fn new(channel: Channel) -> Self {
        Self {
            channel,
            pending: Vec::new(),
        }
    }

    /// The next line including its `\n`, or `None` once the channel closed.
    pub async fn read_line(&mut self, limit: usize) -> io::Result<Option<Vec<u8>>> {
        let mut buf = [0u8; BUFFER_SIZE];
        loop {
            if let Some(pos) = self.pending.iter().position(|&b| b == b'\n') {
                let rest = self.pending.split_off(pos + 1);
                return Ok(Some(std::mem::replace(&mut self.pending, rest)));
            }
            if self.pending.len() > limit {
                return Err(invalid("packet too large"));
            }
            let n = self.channel.read(&mut buf).await?;
            if n == 0 {
                return Ok(None);
            }
            self.pending.extend_from_slice(&buf[..n]);
        }
    }
}

fn frame(kind: u8, id: Uuid, len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER + len);
    out.push(kind);
    out.extend_from_slice(&(len as u16).to_be_bytes());
    out.extend_from_slice(id.as_bytes());
    out
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::time::timeout;

    fn pair() -> (Mux, Mux) {
        let (a, b) = tokio::io::duplex(1 << 16);
        (Mux::new(a), Mux::new(b))
    }

    #[tokio::test]
    async fn packets_on_the_default_channel() {
        let (a, b) = pair();
        a.default_channel().write_all(b"{\"type\":\"x\"}\n{\"type\":\"y\"}\n").await.unwrap();
        let mut lines = LineReader::new(b.default_channel());
        assert_eq!(lines.read_line(1024).await.unwrap().unwrap(), b"{\"type\":\"x\"}\n");
        assert_eq!(lines.read_line(1024).await.unwrap().unwrap(), b"{\"type\":\"y\"}\n");
    }

    #[tokio::test]
    async fn payload_larger_than_the_window() {
        let (a, b) = pair();
        let data: Vec<u8> = (0..100_000u32).map(|i| (i % 253) as u8).collect();
        let out = a.open_channel();
        let id = out.id();

        let sender = {
            let data = data.clone();
            tokio::spawn(async move {
                out.write_all(&data).await.unwrap();
                out.close();
            })
        };

        // The OPEN frame may still be in flight.
        let input = loop {
            if let Some(ch) = b.take_channel(id) {
                break ch;
            }
            tokio::task::yield_now().await;
        };
        let mut got = Vec::new();
        let mut buf = [0u8; 1000];
        loop {
            let n = timeout(Duration::from_secs(5), input.read(&mut buf)).await.unwrap().unwrap();
            if n == 0 {
                break;
            }
            got.extend_from_slice(&buf[..n]);
        }
        sender.await.unwrap();
        assert_eq!(got, data);
    }

    #[tokio::test]
    async fn closing_the_connection_ends_reads() {
        let (a, b) = pair();
        let ch = b.default_channel();
        a.shutdown();
        let mut buf = [0u8; 8];
        let n = timeout(Duration::from_secs(5), ch.read(&mut buf)).await.unwrap().unwrap();
        assert_eq!(n, 0);
        assert!(!b.is_open());
    }

    #[tokio::test]
    async fn rejects_peers_that_skip_the_version() {
        let (raw, other) = tokio::io::duplex(1 << 16);
        let mux = Mux::new(other);
        let (_r, mut w) = tokio::io::split(raw);
        // A READ before any VERSION.
        let mut bad = frame(READ, DEFAULT_CHANNEL, 2);
        bad.extend_from_slice(&10u16.to_be_bytes());
        w.write_all(&bad).await.unwrap();
        let mut buf = [0u8; 1];
        let n = timeout(Duration::from_secs(5), mux.default_channel().read(&mut buf))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn frame_layout() {
        let f = frame(WRITE, DEFAULT_CHANNEL, 3);
        assert_eq!(f[0], 4);
        assert_eq!(&f[1..3], &[0, 3]);
        assert_eq!(&f[3..19], DEFAULT_CHANNEL.as_bytes());
    }
}
