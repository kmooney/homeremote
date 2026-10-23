//! The Companion link: framing over TCP, then encryption once verified.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::appletv::crypto::{SessionCipher, TAG_LEN};
use crate::appletv::frame::{self, FrameType, HEADER_LEN};
use crate::appletv::opack::{self, Value};
use crate::{Error, Result};

/// The device answers promptly or not at all; a long wait just delays the
/// error a user is waiting on.
const IO_TIMEOUT: Duration = Duration::from_secs(8);

/// Refuse absurd frames rather than trying to allocate for them.
const MAX_FRAME: usize = 1024 * 1024;

pub struct Link {
    stream: TcpStream,
    cipher: Option<SessionCipher>,
    /// Correlates replies with requests.
    next_xid: i64,
    /// Bytes read but not yet framed. Reads land here rather than straight
    /// into a frame so that abandoning a read part-way loses nothing, which
    /// is what lets a caller wait for events alongside other work.
    inbox: Vec<u8>,
    /// Events that arrived while a reply was awaited, as (identifier, content).
    events: VecDeque<(String, Value)>,
}

impl Link {
    pub async fn connect(addr: SocketAddr) -> Result<Self> {
        let stream = tokio::time::timeout(IO_TIMEOUT, TcpStream::connect(addr))
            .await
            .map_err(|_| Error::Timeout)??;
        // Remote keypresses are tiny and latency-sensitive.
        let _ = stream.set_nodelay(true);
        Ok(Self {
            stream,
            cipher: None,
            next_xid: 1,
            inbox: Vec::new(),
            events: VecDeque::new(),
        })
    }

    /// Switch to the encrypted session established by pair-verify.
    pub fn enable_encryption(&mut self, out_key: [u8; 32], in_key: [u8; 32]) {
        self.cipher = Some(SessionCipher::new(out_key, in_key));
    }

    pub fn is_encrypted(&self) -> bool {
        self.cipher.is_some()
    }

    pub fn take_xid(&mut self) -> i64 {
        let xid = self.next_xid;
        self.next_xid += 1;
        xid
    }

    /// Send an OPACK message.
    ///
    /// Once encryption is on, the length in the header counts the auth tag and
    /// the header itself is the associated data.
    pub async fn send(&mut self, frame_type: FrameType, message: &Value) -> Result<()> {
        let payload = opack::encode(message)?;

        let (header, body) = match &mut self.cipher {
            // Pairing frames stay in the clear even after keys are set up,
            // though in practice pairing finishes before that happens.
            Some(cipher) if !payload.is_empty() && !frame_type.is_auth() => {
                let header = frame::header(frame_type, payload.len() + TAG_LEN)?;
                let sealed = cipher.encrypt(&payload, &header)?;
                (header, sealed)
            }
            _ => (frame::header(frame_type, payload.len())?, payload),
        };

        tracing::trace!(?frame_type, bytes = body.len(), "-> Apple TV");
        let mut out = Vec::with_capacity(HEADER_LEN + body.len());
        out.extend_from_slice(&header);
        out.extend_from_slice(&body);

        tokio::time::timeout(IO_TIMEOUT, self.stream.write_all(&out))
            .await
            .map_err(|_| Error::Timeout)??;
        Ok(())
    }

    /// Read one frame.
    pub async fn receive(&mut self) -> Result<(FrameType, Value)> {
        loop {
            if let Some(frame) = self.take_frame()? {
                return Ok(frame);
            }
            tokio::time::timeout(IO_TIMEOUT, self.fill())
                .await
                .map_err(|_| Error::Timeout)??;
        }
    }

    /// Wait for the next event the device sends on its own.
    ///
    /// Has no timeout, since an idle device sends nothing, and is cancel-safe:
    /// a frame is only ever consumed in full, so this can sit in a `select!`.
    pub async fn next_event(&mut self) -> Result<(String, Value)> {
        loop {
            if let Some(event) = self.events.pop_front() {
                return Ok(event);
            }
            while let Some((frame_type, message)) = self.take_frame()? {
                if is_event(frame_type, &message) {
                    self.queue_event(message);
                } else {
                    tracing::debug!(?frame_type, "ignoring a frame nothing was waiting for");
                }
            }
            if self.events.is_empty() {
                self.fill().await?;
            }
        }
    }

    /// Read whatever has arrived into the inbox.
    async fn fill(&mut self) -> Result<()> {
        self.inbox.reserve(4096);
        // Cancel-safe: if abandoned, no bytes were taken off the socket.
        if self.stream.read_buf(&mut self.inbox).await? == 0 {
            return Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "the Apple TV closed the connection",
            )));
        }
        Ok(())
    }

    /// Take one complete frame off the inbox, if one has fully arrived.
    fn take_frame(&mut self) -> Result<Option<(FrameType, Value)>> {
        let Some(header) = self.inbox.first_chunk::<HEADER_LEN>().copied() else {
            return Ok(None);
        };
        let (frame_type, length) = frame::parse_header(&header);
        if length > MAX_FRAME {
            return Err(Error::Protocol(format!(
                "device announced an implausible {length}-byte frame"
            )));
        }
        if self.inbox.len() < HEADER_LEN + length {
            return Ok(None);
        }
        let mut body = self.inbox[HEADER_LEN..HEADER_LEN + length].to_vec();
        self.inbox.drain(..HEADER_LEN + length);
        tracing::trace!(?frame_type, bytes = length, "<- Apple TV");

        if let Some(cipher) = &mut self.cipher
            && !body.is_empty()
            && !frame_type.is_auth()
        {
            body = cipher.decrypt(&body, &header)?;
        }

        if body.is_empty() {
            return Ok(Some((frame_type, Value::Null)));
        }
        Ok(Some((frame_type, opack::decode(&body)?)))
    }

    fn queue_event(&mut self, message: Value) {
        let identifier = message.get("_i").and_then(Value::as_str).unwrap_or_default();
        tracing::debug!(identifier, "<- event");
        let content = message.get("_c").cloned().unwrap_or(Value::Null);
        self.events.push_back((identifier.to_string(), content));
    }

    /// Read the reply to the frame in front of it, setting aside any events
    /// that arrive first.
    async fn receive_reply(&mut self) -> Result<Value> {
        loop {
            let (frame_type, message) = self.receive().await?;
            if is_event(frame_type, &message) {
                self.queue_event(message);
                continue;
            }
            if let Some(error) = frame::error_message(&message) {
                return Err(Error::Protocol(format!("device rejected the request: {error}")));
            }
            return Ok(message);
        }
    }

    /// Send a message and wait for its reply.
    ///
    /// The device replies to the frame in front of it, so this stays a simple
    /// exchange rather than a correlated queue; only events, which it sends
    /// whenever it likes, have to be stepped over.
    pub async fn exchange(
        &mut self,
        frame_type: FrameType,
        message: &Value,
    ) -> Result<Value> {
        self.send(frame_type, message).await?;
        self.receive_reply().await
    }

    /// Send a command without waiting for its reply.
    ///
    /// Lets a caller put several frames on the wire before collecting any
    /// answers. The device replies in order, so replies are matched by
    /// position rather than by transaction id.
    pub async fn send_command(&mut self, identifier: &str, content: Value) -> Result<()> {
        let xid = self.take_xid();
        tracing::debug!(identifier, xid, "command -> (pipelined)");
        let message = frame::request(identifier, xid, content);
        self.send(FrameType::EOpack, &message).await
    }

    /// Send a one-way event, which the device does not answer.
    pub async fn send_event(&mut self, identifier: &str, content: Value) -> Result<()> {
        let xid = self.take_xid();
        tracing::debug!(identifier, xid, "event ->");
        let message = frame::event(identifier, xid, content);
        self.send(FrameType::EOpack, &message).await
    }

    /// Collect one reply previously sent with [`Link::send_command`].
    pub async fn await_reply(&mut self) -> Result<Value> {
        let reply = self.receive_reply().await?;
        Ok(reply.get("_c").cloned().unwrap_or(Value::Null))
    }

    /// Send a `_i`/`_t`/`_c` command and return its content.
    pub async fn command(&mut self, identifier: &str, content: Value) -> Result<Value> {
        let xid = self.take_xid();
        tracing::debug!(identifier, xid, encrypted = self.is_encrypted(), "command ->");
        let message = frame::request(identifier, xid, content);
        let reply = self.exchange(FrameType::EOpack, &message).await?;
        tracing::debug!(identifier, "command <- ok");
        Ok(reply.get("_c").cloned().unwrap_or(Value::Null))
    }
}

/// Whether a frame is an event the device sent unprompted, rather than a reply.
///
/// Events can land between a request and its reply, so anything matching
/// replies by position has to step over them.
fn is_event(frame_type: FrameType, message: &Value) -> bool {
    frame_type == FrameType::EOpack
        && message.get("_t").and_then(Value::as_int) == Some(frame::EVENT)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::appletv::opack::dict;
    use tokio::net::TcpListener;

    fn encoded(message: &Value) -> Vec<u8> {
        let body = opack::encode(message).unwrap();
        let mut out = frame::header(FrameType::EOpack, body.len()).unwrap().to_vec();
        out.extend(body);
        out
    }

    /// A peer that answers the first request with an event, then the reply.
    async fn device_that_interjects() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut header = [0u8; HEADER_LEN];
            socket.read_exact(&mut header).await.unwrap();
            let (_, length) = frame::parse_header(&header);
            socket.read_exact(&mut vec![0u8; length]).await.unwrap();

            let event = frame::event("_tiStarted", 99, dict([("_tiD", Value::Bytes(vec![1]))]));
            let mut reply = BTreeMap::new();
            reply.insert("_t".to_string(), Value::Int(frame::RESPONSE));
            reply.insert("_x".to_string(), Value::Int(1));
            reply.insert("_c".to_string(), dict([("ok", Value::Int(1))]));

            let mut out = encoded(&event);
            out.extend(encoded(&Value::Dict(reply)));
            // Split mid-frame so reassembly across reads is exercised too.
            let (first, second) = out.split_at(7);
            socket.write_all(first).await.unwrap();
            socket.flush().await.unwrap();
            tokio::time::sleep(Duration::from_millis(20)).await;
            socket.write_all(second).await.unwrap();
            // Hold the connection open until the test is done with it.
            tokio::time::sleep(Duration::from_secs(5)).await;
        });
        addr
    }

    #[tokio::test]
    async fn an_event_ahead_of_a_reply_is_set_aside_not_mistaken_for_it() {
        let mut link = Link::connect(device_that_interjects().await).await.unwrap();

        let reply = link.command("_tiStart", Value::Null).await.unwrap();
        assert_eq!(reply.get("ok").and_then(Value::as_int), Some(1));

        let (identifier, content) = link.next_event().await.unwrap();
        assert_eq!(identifier, "_tiStarted");
        assert_eq!(content.get("_tiD").and_then(Value::as_bytes), Some(&[1u8][..]));
    }

    #[tokio::test]
    async fn waiting_for_events_can_be_abandoned_without_losing_data() {
        let addr = device_that_interjects().await;
        let mut link = Link::connect(addr).await.unwrap();
        link.send_command("_tiStart", Value::Null).await.unwrap();

        // Give up on the event wait repeatedly, including mid-frame.
        for _ in 0..3 {
            let _ = tokio::time::timeout(Duration::from_millis(5), link.next_event()).await;
        }
        let (identifier, _) = link.next_event().await.unwrap();
        assert_eq!(identifier, "_tiStarted");
    }
}
