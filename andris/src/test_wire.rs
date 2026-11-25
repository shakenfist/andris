//! Reading the framed messages a channel sent, for unit tests.
//!
//! A channel writes mini-header framing into a stream, so a test that
//! wants one message has to read chunks until the buffer can reassemble
//! one. Three of them wrote that loop out: `main_channel`'s pings test
//! and its served-channel harness, and `inputs`'s wait for `INIT`, each
//! with a chunk size of its own and the same four lines of
//! timeout-read-assert-extend. This is that loop, once.

use std::time::Duration;

use shakenfist_spice_protocol::messages::{ReceiveBuffer, ReceivedMessage};
use tokio::io::{AsyncRead, AsyncReadExt};

/// The largest message these tests read. Far above anything a channel
/// sends them, so it never decides the outcome.
const MAX_MESSAGE: usize = 1 << 20;

/// How much is read at a time. Reassembly makes the size invisible to
/// the caller: a message split across chunks is put back together, and
/// a chunk holding several is taken apart.
const CHUNK: usize = 4096;

/// Reassembles the messages a channel sent.
///
/// It holds the buffer, not the stream, so a test can keep writing to
/// its end of the stream and drop it when it wants to: the stream is
/// passed to [`WireReader::recv`] each time.
pub struct WireReader {
    buf: ReceiveBuffer,
    wait: Duration,
}

impl WireReader {
    /// A reader that gives a read at most `wait` to arrive.
    pub fn new(wait: Duration) -> Self {
        Self {
            buf: ReceiveBuffer::new(),
            wait,
        }
    }

    /// The next message, reading from `stream` until there is one.
    ///
    /// Panics, as a test helper should, if the wait runs out, the read
    /// fails, the channel closes first, or the framing is bad.
    pub async fn recv<R: AsyncRead + Unpin>(&mut self, stream: &mut R) -> ReceivedMessage {
        loop {
            if let Some(message) = self.buf.take_message(MAX_MESSAGE).unwrap() {
                return message;
            }
            let mut chunk = [0u8; CHUNK];
            let read = tokio::time::timeout(self.wait, stream.read(&mut chunk))
                .await
                .expect("the channel sent nothing before the wait ran out")
                .expect("reading from the channel failed");
            assert!(read > 0, "the channel closed");
            self.buf.extend_from_slice(&chunk[..read]);
        }
    }
}
