//! Per-connection console: the daemon's own TUI, streamed to a client.
//!
//! A console is a PTY running `construct --socket <this daemon> tui`. The
//! browser renders its bytes in an xterm and types back into it, so every
//! TUI surface — session list, splits, playbook, palette — is reachable from
//! a web client without the web client reimplementing it.
//!
//! A console is deliberately **not** a session. It is never listed,
//! persisted, counted in fleet tallies, or resumed after a restart: it is a
//! client attached to this daemon that happens to run on the daemon's host.
//! Its lifetime is exactly its owning connection's, so closing the browser
//! tab reaps the process.
//!
//! There is one console per connection and therefore exactly one viewer, so
//! none of the shared-PTY geometry arbitration that sessions need (spec 0153)
//! applies here: the viewer's fit is simply the size.

use base64::Engine as _;
use construct_protocol::{ipc_notif, ConsoleExitParams, ConsoleOutputParams, Notification};
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use std::io::{Read, Write};
use std::sync::Mutex;
use tokio::sync::mpsc;

const READ_BUF: usize = 8 * 1024;

/// Environment the console client inherits beyond the daemon's own. `TERM`
/// has to be something xterm.js actually implements; the daemon may have been
/// started from a launchd plist with no `TERM` at all.
fn console_term() -> String {
    match std::env::var("TERM") {
        Ok(t) if !t.is_empty() && t != "dumb" => t,
        _ => "xterm-256color".to_string(),
    }
}

struct Running {
    /// Bytes to write to the PTY master. A dedicated blocking thread drains
    /// it, since `portable_pty`'s writer is not async.
    write_tx: mpsc::UnboundedSender<Vec<u8>>,
    master: Box<dyn MasterPty + Send>,
    killer: Box<dyn portable_pty::ChildKiller + Send + Sync>,
}

/// A connection's console slot. Created empty for every client connection;
/// holds at most one console at a time.
pub struct ConsoleSlot {
    inner: Mutex<Option<Running>>,
}

impl ConsoleSlot {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(None),
        }
    }

    pub fn is_open(&self) -> bool {
        self.inner.lock().map(|g| g.is_some()).unwrap_or(false)
    }

    /// Spawn the console client. Returns `Ok(false)` when this connection
    /// already had one open, in which case the call only applies the size.
    pub fn open(
        &self,
        out_tx: mpsc::UnboundedSender<serde_json::Value>,
        cols: u16,
        rows: u16,
    ) -> anyhow::Result<bool> {
        if self.is_open() {
            self.resize(cols, rows);
            return Ok(false);
        }

        let exe = std::env::current_exe().map_err(|e| anyhow::anyhow!("resolve construct: {e}"))?;
        let socket = construct_protocol::paths::Paths::discover().socket();

        let size = PtySize {
            cols: cols.max(20),
            rows: rows.max(5),
            pixel_width: 0,
            pixel_height: 0,
        };
        let pair = native_pty_system()
            .openpty(size)
            .map_err(|e| anyhow::anyhow!("openpty: {e}"))?;

        let mut cmd = CommandBuilder::new(&exe);
        cmd.arg("--socket");
        cmd.arg(&socket);
        cmd.arg("tui");
        // The daemon's cwd is as good a default as any; the TUI creates
        // sessions with explicitly chosen working directories anyway.
        if let Ok(cwd) = std::env::current_dir() {
            cmd.cwd(cwd);
        }
        cmd.env("TERM", console_term());
        // Mark the client so it can tell it is being rendered through a
        // browser rather than a real terminal — nothing branches on this yet,
        // but a console has no OS clipboard and no user terminal to hand
        // OSC 52 to, which is exactly the kind of thing that will need it.
        cmd.env("CONSTRUCT_CONSOLE", "1");

        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| anyhow::anyhow!("spawn console client: {e}"))?;
        let killer = child.clone_killer();
        let master = pair.master;
        let slave = pair.slave;

        let reader = master
            .try_clone_reader()
            .map_err(|e| anyhow::anyhow!("console pty reader: {e}"))?;
        let writer = master
            .take_writer()
            .map_err(|e| anyhow::anyhow!("console pty writer: {e}"))?;

        // PTY → connection. The read side is blocking, so it lives on its own
        // thread and hands chunks to the async writer channel.
        let out_for_reader = out_tx.clone();
        std::thread::spawn(move || {
            let mut reader = reader;
            let mut buf = vec![0u8; READ_BUF];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let data =
                            base64::engine::general_purpose::STANDARD.encode(&buf[..n]);
                        let notif = Notification::new(
                            ipc_notif::CONSOLE_OUTPUT,
                            serde_json::to_value(ConsoleOutputParams { data }).ok(),
                        );
                        match serde_json::to_value(&notif) {
                            Ok(v) => {
                                if out_for_reader.send(v).is_err() {
                                    break;
                                }
                            }
                            Err(_) => break,
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        let (write_tx, mut write_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        std::thread::spawn(move || {
            let mut writer = writer;
            while let Some(bytes) = write_rx.blocking_recv() {
                if writer.write_all(&bytes).is_err() {
                    break;
                }
                let _ = writer.flush();
            }
        });

        // Reap the child and tell the client. Holding the slave until the
        // child is waited on keeps the PTY from collapsing early.
        tokio::task::spawn_blocking(move || {
            let _slave_alive = slave;
            let mut child = child;
            let code = child.wait().map(|s| s.exit_code() as i32).unwrap_or(-1);
            let notif = Notification::new(
                ipc_notif::CONSOLE_EXIT,
                serde_json::to_value(ConsoleExitParams { exit_code: code }).ok(),
            );
            if let Ok(v) = serde_json::to_value(&notif) {
                let _ = out_tx.send(v);
            }
        });

        if let Ok(mut guard) = self.inner.lock() {
            *guard = Some(Running {
                write_tx,
                master,
                killer,
            });
        }
        Ok(true)
    }

    pub fn input(&self, bytes: Vec<u8>) {
        if let Ok(guard) = self.inner.lock() {
            if let Some(running) = guard.as_ref() {
                let _ = running.write_tx.send(bytes);
            }
        }
    }

    pub fn resize(&self, cols: u16, rows: u16) {
        if let Ok(guard) = self.inner.lock() {
            if let Some(running) = guard.as_ref() {
                let _ = running.master.resize(PtySize {
                    cols: cols.max(20),
                    rows: rows.max(5),
                    pixel_width: 0,
                    pixel_height: 0,
                });
            }
        }
    }

    /// Kill the console client. Idempotent — closing a slot that never opened
    /// one, or whose child already exited, is a no-op.
    pub fn close(&self) {
        let taken = self.inner.lock().ok().and_then(|mut g| g.take());
        if let Some(mut running) = taken {
            let _ = running.killer.kill();
        }
    }
}

impl Default for ConsoleSlot {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for ConsoleSlot {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn console_term_falls_back_when_unusable() {
        // Whatever the daemon's own TERM is, the console must never inherit
        // `dumb` — xterm.js renders a full-screen TUI or nothing.
        assert_ne!(console_term(), "dumb");
        assert!(!console_term().is_empty());
    }

    #[test]
    fn close_is_idempotent_on_an_unopened_slot() {
        let slot = ConsoleSlot::new();
        assert!(!slot.is_open());
        slot.close();
        slot.close();
        assert!(!slot.is_open());
    }
}
