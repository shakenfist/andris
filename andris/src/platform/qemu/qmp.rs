//! Just enough of QMP, qemu's JSON monitor protocol, to hand qemu one end
//! of a socket as a D-Bus display client.
//!
//! QMP sends one JSON object a line: a greeting, then a `return` or an
//! `error` for each command, with asynchronous `event`s interleaved. A
//! file descriptor travels with the command that names it (`getfd`), as
//! `SCM_RIGHTS` ancillary data.

use std::io::{BufRead, BufReader, IoSlice, Read, Write};
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use rustix::net::{SendAncillaryBuffer, SendAncillaryMessage, SendFlags};
use serde_json::{Value, json};

/// The longest line andris reads from QMP, in bytes. Replies to the
/// commands andris sends are short; a longer line is not QMP.
const MAX_LINE: usize = 64 * 1024;

/// A QMP connection, past the greeting and capabilities negotiation.
pub struct Qmp {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

impl Qmp {
    /// Connect to the QMP socket at `path`, read the greeting and leave
    /// capabilities negotiation. Each read and each write waits at most
    /// `timeout`, so a qemu that stops answering cannot hold andris for
    /// ever.
    pub fn connect(path: &Path, timeout: Duration) -> Result<Self> {
        let stream = UnixStream::connect(path)
            .with_context(|| format!("connecting to qemu's QMP socket {}", path.display()))?;
        // The timeouts belong to the socket, so the writer's clone below
        // shares them.
        stream
            .set_read_timeout(Some(timeout))
            .context("setting a read timeout on the QMP socket")?;
        stream
            .set_write_timeout(Some(timeout))
            .context("setting a write timeout on the QMP socket")?;
        let writer = stream.try_clone().context("cloning the QMP socket")?;
        let mut qmp = Self {
            reader: BufReader::new(stream),
            writer,
        };
        let greeting = qmp.read_message()?;
        if greeting.get("QMP").is_none() {
            bail!("{} did not greet as QMP: {greeting}", path.display());
        }
        qmp.execute(&json!({"execute": "qmp_capabilities"}), None)?;
        Ok(qmp)
    }

    /// Give qemu `fd`, under `name`, for a later command to use.
    pub fn getfd(&mut self, name: &str, fd: BorrowedFd<'_>) -> Result<()> {
        self.execute(
            &json!({"execute": "getfd", "arguments": {"fdname": name}}),
            Some(fd),
        )
        .map(drop)
        .with_context(|| format!("passing qemu the descriptor {name}"))
    }

    /// Make the socket qemu holds as `name` a client of its D-Bus
    /// display. qemu must run `-display dbus,p2p=on`.
    pub fn add_dbus_display_client(&mut self, name: &str) -> Result<()> {
        self.execute(
            &json!({
                "execute": "add_client",
                "arguments": {"protocol": "@dbus-display", "fdname": name},
            }),
            None,
        )
        .map(drop)
        .context(
            "adding a D-Bus display client; qemu must run with -display dbus,p2p=on for andris \
             to attach to it",
        )
    }

    /// Send `command`, with `fd` attached if given, and wait for its
    /// reply, skipping events. Fails if qemu replies with an error.
    fn execute(&mut self, command: &Value, fd: Option<BorrowedFd<'_>>) -> Result<Value> {
        let mut line = command.to_string();
        line.push('\n');
        match fd {
            None => self
                .writer
                .write_all(line.as_bytes())
                .context("writing to QMP")?,
            Some(fd) => send_with_fd(&self.writer, line.as_bytes(), fd)?,
        }
        loop {
            let reply = self.read_message()?;
            if let Some(value) = reply.get("return") {
                return Ok(value.clone());
            }
            if let Some(error) = reply.get("error") {
                let desc = error
                    .get("desc")
                    .and_then(Value::as_str)
                    .unwrap_or("no description");
                bail!("qemu refused {}: {desc}", command["execute"]);
            }
            // An event; the reply is still to come.
        }
    }

    /// Read one JSON object.
    fn read_message(&mut self) -> Result<Value> {
        let mut line = String::new();
        let read = (&mut self.reader)
            .take(MAX_LINE as u64)
            .read_line(&mut line)
            .context("reading from QMP")?;
        if read == 0 {
            bail!("qemu closed its QMP socket");
        }
        if !line.ends_with('\n') {
            bail!("QMP sent a line longer than {MAX_LINE} bytes, or an unfinished one");
        }
        serde_json::from_str(&line).with_context(|| format!("QMP sent something not JSON: {line}"))
    }
}

/// Write all of `bytes` to `stream`, with `fd` attached to the first
/// write as `SCM_RIGHTS`.
fn send_with_fd(stream: &UnixStream, bytes: &[u8], fd: BorrowedFd<'_>) -> Result<()> {
    let mut space = [MaybeUninit::<u8>::uninit(); rustix::cmsg_space!(ScmRights(1))];
    let mut control = SendAncillaryBuffer::new(&mut space);
    let fds = [fd];
    if !control.push(SendAncillaryMessage::ScmRights(&fds)) {
        bail!("no room to attach a descriptor to a QMP command");
    }
    let sent = rustix::net::sendmsg(
        stream.as_fd(),
        &[IoSlice::new(bytes)],
        &mut control,
        SendFlags::empty(),
    )
    .context("writing a QMP command with a descriptor")?;
    // The descriptor went with the first byte; the rest, if a short write
    // left any, goes plainly.
    let mut writer = stream;
    writer
        .write_all(&bytes[sent..])
        .context("writing the rest of a QMP command")
}

#[cfg(test)]
mod tests {
    use std::os::unix::net::UnixListener;

    use super::*;

    /// Play qemu for one QMP session at a socket in `dir`: greet, accept
    /// capabilities, then answer each later line with `replies` in turn.
    /// Returns the socket's path and the lines received.
    fn fake_qmp(
        dir: &Path,
        replies: Vec<&'static str>,
    ) -> (std::path::PathBuf, std::thread::JoinHandle<Vec<String>>) {
        let path = dir.join("qmp.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let handle = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut writer = stream.try_clone().unwrap();
            let mut reader = BufReader::new(stream);
            writer
                .write_all(b"{\"QMP\": {\"version\": {}, \"capabilities\": []}}\r\n")
                .unwrap();
            let mut seen = Vec::new();
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            seen.push(line.clone());
            writer.write_all(b"{\"return\": {}}\r\n").unwrap();
            for reply in replies {
                line.clear();
                reader.read_line(&mut line).unwrap();
                seen.push(line.clone());
                writer.write_all(reply.as_bytes()).unwrap();
            }
            // Drain until andris hangs up.
            let mut rest = Vec::new();
            let _ = reader.read_to_end(&mut rest);
            seen
        });
        (path, handle)
    }

    #[test]
    fn a_qemu_that_never_greets_times_out() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("qmp.sock");
        let _listener = UnixListener::bind(&path).unwrap();
        let error = Qmp::connect(&path, Duration::from_millis(100))
            .err()
            .unwrap();
        assert!(
            format!("{error:#}").contains("reading from QMP"),
            "{error:#}"
        );
    }

    #[test]
    fn commands_skip_events_and_report_errors() {
        let dir = tempfile::tempdir().unwrap();
        let (path, server) = fake_qmp(
            dir.path(),
            vec![
                "{\"event\": \"RESUME\"}\r\n{\"return\": {}}\r\n",
                "{\"error\": {\"class\": \"GenericError\", \"desc\": \"no dbus\"}}\r\n",
            ],
        );
        let mut qmp = Qmp::connect(&path, Duration::from_secs(5)).unwrap();
        let (ours, _theirs) = UnixStream::pair().unwrap();
        qmp.getfd("display", ours.as_fd()).unwrap();
        let error = qmp.add_dbus_display_client("display").unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("no dbus"), "{message}");
        assert!(message.contains("p2p=on"), "{message}");
        drop(qmp);
        let seen = server.join().unwrap();
        assert!(seen[0].contains("qmp_capabilities"));
        assert!(seen[1].contains("\"getfd\"") && seen[1].contains("display"));
        assert!(seen[2].contains("@dbus-display"));
    }
}
