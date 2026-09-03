//! The Companion link: framing over TCP, then encryption once verified.

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
        let mut header = [0u8; HEADER_LEN];
        tokio::time::timeout(IO_TIMEOUT, self.stream.read_exact(&mut header))
            .await
            .map_err(|_| Error::Timeout)??;
        let (frame_type, length) = frame::parse_header(&header);
        // Logged before the body so a stalled read is distinguishable from
        // nothing arriving at all.
        tracing::debug!(?frame_type, length, "<- header");

        if length > MAX_FRAME {
            return Err(Error::Protocol(format!(
                "device announced an implausible {length}-byte frame"
            )));
        }

        let mut body = vec![0u8; length];
        if length > 0 {
            tokio::time::timeout(IO_TIMEOUT, self.stream.read_exact(&mut body))
                .await
                .map_err(|_| Error::Timeout)??;
        }
        tracing::trace!(?frame_type, bytes = length, "<- Apple TV");

        if let Some(cipher) = &mut self.cipher
            && !body.is_empty()
            && !frame_type.is_auth()
        {
            body = cipher.decrypt(&body, &header)?;
        }

        if body.is_empty() {
            return Ok((frame_type, Value::Null));
        }
        Ok((frame_type, opack::decode(&body)?))
    }

    /// Send a message and wait for the next frame back.
    ///
    /// The device replies to the frame in front of it, so this stays a simple
    /// exchange rather than a correlated queue.
    pub async fn exchange(
        &mut self,
        frame_type: FrameType,
        message: &Value,
    ) -> Result<Value> {
        self.send(frame_type, message).await?;
        let (_, reply) = self.receive().await?;
        if let Some(error) = frame::error_message(&reply) {
            return Err(Error::Protocol(format!("device rejected the request: {error}")));
        }
        Ok(reply)
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
        let (_, reply) = self.receive().await?;
        if let Some(error) = frame::error_message(&reply) {
            return Err(Error::Protocol(format!("device rejected the request: {error}")));
        }
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
