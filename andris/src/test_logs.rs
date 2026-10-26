//! Log lines, collected for a unit test to read.

use std::sync::{Arc, Mutex};

use tracing::Level;
use tracing::subscriber::DefaultGuard;

/// The log lines written while a [`Logs::capture`] guard was held, on
/// that thread.
#[derive(Clone, Default)]
pub struct Logs(Arc<Mutex<Vec<u8>>>);

impl Logs {
    /// Collect this thread's log lines at `level` and above, without
    /// colour, until the guard is dropped.
    pub fn capture(&self, level: Level) -> DefaultGuard {
        tracing::subscriber::set_default(
            tracing_subscriber::fmt()
                .with_writer(self.clone())
                .with_max_level(level)
                .with_ansi(false)
                .finish(),
        )
    }

    /// Everything collected so far.
    pub fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}

impl std::io::Write for Logs {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Logs {
    type Writer = Logs;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}
