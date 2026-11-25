//! Reading the framed messages a channel sent, for unit tests.
//!
//! A channel writes mini-header framing into a stream, so a test that
//! wants one message has to read chunks until the buffer can reassemble
//! one. This is that loop, in one place: reassembly makes the chunk size
//! invisible, so a message split across reads is put back together and a
//! read holding several is taken apart.

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

#[cfg(test)]
mod tests {
    use shakenfist_spice_protocol::messages::make_message;

    use super::*;

    /// A reader whose reads are at most `most` bytes, so that the chunk
    /// boundaries fall inside messages rather than between them.
    struct Dribble {
        bytes: Vec<u8>,
        at: usize,
        most: usize,
    }

    impl AsyncRead for Dribble {
        fn poll_read(
            mut self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            let left = &self.bytes[self.at..];
            let take = left.len().min(self.most).min(buf.remaining());
            buf.put_slice(&left[..take]);
            self.at += take;
            std::task::Poll::Ready(Ok(()))
        }
    }

    /// Three messages, in one buffer.
    fn three() -> Vec<u8> {
        let mut bytes = Vec::new();
        for (opcode, body) in [(101u16, &b"first"[..]), (102, b"second"), (103, b"third")] {
            bytes.extend_from_slice(&make_message(opcode, body));
        }
        bytes
    }

    /// The claim the module is for: the chunk size is invisible. One byte
    /// at a time splits every message across reads; a large read holds
    /// all three at once. Either way the same three come back.
    #[tokio::test]
    async fn reassembles_whatever_the_reads_divide() {
        for most in [1, 7, 4096] {
            let mut stream = Dribble {
                bytes: three(),
                at: 0,
                most,
            };
            let mut wire = WireReader::new(Duration::from_secs(5));
            let mut got = Vec::new();
            for _ in 0..3 {
                let message = wire.recv(&mut stream).await;
                got.push((message.header.message_type, message.payload().to_vec()));
            }
            assert_eq!(
                got,
                vec![
                    (101, b"first".to_vec()),
                    (102, b"second".to_vec()),
                    (103, b"third".to_vec()),
                ],
                "reads of at most {most} bytes"
            );
        }
    }
}
