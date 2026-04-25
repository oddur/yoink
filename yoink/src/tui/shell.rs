//! Embedded interactive shell. Drops the operator into a PTY-mode
//! `docker exec` (default `bash`, falls back to `sh`) without leaving
//! the TUI. Bytes from the exec output stream are fed into a
//! `vt100::Parser`; ratatui renders the parser's screen each frame via
//! the `tui-term` widget. Key events are encoded back to bytes and
//! written to the exec stdin sink.
//!
//! Lifecycle: `ShellState::start` spawns the exec and a single bridge
//! task that pumps the docker output channel into an mpsc that the
//! TUI event loop drains on each `fast_tick`. Resize is sent to the
//! daemon whenever the rendered panel size changes.
//!
//! "k9s drop-into-container" is the UX target: enter from a container
//! row → land at a shell prompt → Ctrl-D / `exit` exits → return to
//! the previous pane.

use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use futures_util::StreamExt;
use ratatui::Frame;
use ratatui::style::{Color, Style};
use ratatui::widgets::{Block, Borders, Paragraph};
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::warn;
use tui_term::widget::PseudoTerminal;

use crate::docker_ops::{DockerOps, ExecKind, ExecSession, Host};

use super::ui::pane_layout;

/// Default scrollback the parser keeps. 5 000 lines mirrors the logs
/// pane and is plenty for diagnosing a misbehaving service.
const SCROLLBACK: usize = 5_000;

pub struct ShellState {
    host: Host,
    container: String,
    /// `None` until `start` succeeds; if start failed we render the
    /// error in the panel rather than crashing the TUI.
    inner: Option<ShellInner>,
    /// Set when the exec output stream `EOFed` (the container shell
    /// exited). The next key press returns the user to the previous
    /// pane.
    exited: bool,
    error: Option<String>,
}

struct ShellInner {
    parser: vt100::Parser,
    /// `AsyncWrite` into docker exec stdin. Wrapped in a tokio Mutex so
    /// the event-handler can fire-and-forget keystroke writes from any
    /// task spawn.
    stdin: Arc<tokio::sync::Mutex<std::pin::Pin<Box<dyn tokio::io::AsyncWrite + Send>>>>,
    /// EOF sentinel — the bridge holds the matching sender; when it
    /// drops, this channel disconnects and `drain_output` notices.
    eof_rx: mpsc::UnboundedReceiver<()>,
    /// Exec id for `Exec`, container name for `Sidecar`.
    id: String,
    kind: ExecKind,
    /// Last (rows, cols) we sent to the daemon; we re-send on change.
    last_size: (u16, u16),
    /// Flipped true after the bridge delivers the first byte from the
    /// daemon. Resize calls before this race against bollard's exec
    /// plumbing and surface a noisy "exec process is not started" 400.
    started: bool,
    bridge: JoinHandle<()>,
}

impl Drop for ShellInner {
    fn drop(&mut self) {
        self.bridge.abort();
    }
}

impl ShellState {
    pub fn new(host: Host, container: String) -> Self {
        Self {
            host,
            container,
            inner: None,
            exited: false,
            error: None,
        }
    }

    /// True once the embedded shell has exited (Ctrl-D, `exit`, or
    /// container died). The app loop uses this to bounce back to the
    /// previous pane on the next key.
    pub fn exited(&self) -> bool {
        self.exited
    }

    /// Spawn the docker exec and start the byte-pump. `rows`/`cols`
    /// are the inside-the-border dimensions of the panel — pass them
    /// at start time so the shell prompts at the right size on the
    /// first frame. `bytes_tx` is the app-level mpsc that wakes the
    /// event loop on every byte chunk, so the render fires as soon as
    /// the daemon sends output (rather than waiting for the 2s tick).
    pub async fn start(
        &mut self,
        ops: Arc<dyn DockerOps>,
        bytes_tx: mpsc::UnboundedSender<Vec<u8>>,
        rows: u16,
        cols: u16,
    ) {
        // Pick the shell *inside* the container — exec'ing `bash`
        // directly returns OK at the API level even when the binary's
        // missing (the daemon then reports an OCI exec failure on the
        // output stream), so an outer bash→sh fallback never fires.
        // Hardened/distroless/Alpine images frequently ship `sh` only;
        // this one-liner picks bash when present and falls back to sh
        // otherwise. If neither exists the container has no usable
        // shell and we surface the resulting error in the panel.
        let cmd = vec![
            "/bin/sh".into(),
            "-c".into(),
            "if command -v bash >/dev/null 2>&1; then exec bash; else exec /bin/sh; fi".into(),
        ];
        let session = match ops
            .exec_interactive(&self.host, &self.container, cmd, rows, cols)
            .await
        {
            Ok(s) => s,
            Err(e) => {
                self.error = Some(format!("failed to start shell: {e}"));
                return;
            }
        };
        let banner = format!(
            "\x1b[2myoink shell · {} · {}\x1b[0m\r\n",
            self.host.address, self.container
        );
        self.wire_session(session, bytes_tx, rows, cols, &banner);
    }

    /// Variant of `start` that spawns a debug sidecar container in the
    /// target's pid+net namespaces — for distroless / shell-less images.
    pub async fn start_debug(
        &mut self,
        ops: Arc<dyn DockerOps>,
        bytes_tx: mpsc::UnboundedSender<Vec<u8>>,
        image: &str,
        rows: u16,
        cols: u16,
    ) {
        let session = match ops
            .start_debug_sidecar(&self.host, &self.container, image, rows, cols)
            .await
        {
            Ok(s) => s,
            Err(e) => {
                self.error = Some(format!("failed to start debug sidecar: {e}"));
                return;
            }
        };
        let banner = format!(
            "\x1b[2myoink debug sidecar ({image}) · target {}\x1b[0m\r\n",
            self.container
        );
        self.wire_session(session, bytes_tx, rows, cols, &banner);
    }

    fn wire_session(
        &mut self,
        session: ExecSession,
        bytes_tx: mpsc::UnboundedSender<Vec<u8>>,
        rows: u16,
        cols: u16,
        banner: &str,
    ) {
        let ExecSession {
            id,
            kind,
            stdin,
            mut output,
        } = session;

        // EOF sentinel: dropped when the bridge exits so the event
        // loop notices the shell ended (Ctrl-D / `exit` / sidecar
        // auto-removed) without us having to poll the byte channel.
        let (eof_tx, eof_rx) = mpsc::unbounded_channel::<()>();
        let bridge = tokio::spawn(async move {
            let _eof_guard = eof_tx;
            while let Some(item) = output.next().await {
                match item {
                    Ok(bytes) if bytes.is_empty() => {}
                    Ok(bytes) => {
                        if bytes_tx.send(bytes.to_vec()).is_err() {
                            return;
                        }
                    }
                    Err(e) => {
                        warn!(error = %e, "exec output stream error");
                        return;
                    }
                }
            }
        });

        let mut parser = vt100::Parser::new(rows, cols, SCROLLBACK);
        parser.process(banner.as_bytes());

        self.inner = Some(ShellInner {
            parser,
            stdin: Arc::new(tokio::sync::Mutex::new(stdin)),
            id,
            kind,
            last_size: (rows, cols),
            started: false,
            eof_rx,
            bridge,
        });
    }

    /// Feed one chunk of bytes (delivered by the app's bytes channel)
    /// into the vt100 parser. Called per chunk so the render fires as
    /// soon as the daemon emits output.
    pub fn process_bytes(&mut self, bytes: &[u8]) {
        if let Some(inner) = self.inner.as_mut() {
            inner.parser.process(bytes);
            inner.started = true;
        }
    }

    /// Check the EOF sentinel — when the bridge task drops it, the
    /// container shell has exited. Called from the fast tick.
    pub fn poll_exit(&mut self) {
        let Some(inner) = self.inner.as_mut() else {
            return;
        };
        if matches!(
            inner.eof_rx.try_recv(),
            Err(mpsc::error::TryRecvError::Disconnected)
        ) {
            self.exited = true;
        }
    }

    /// Forward a key event from crossterm into the docker exec stdin.
    /// Returns `true` if the key was an "exit the shell" gesture
    /// (Ctrl-Q — Ctrl-C/Ctrl-D are forwarded into the shell so they
    /// behave normally there).
    pub fn handle_key(&mut self, key: KeyEvent, ops: Arc<dyn DockerOps>) -> bool {
        if self.exited {
            return true;
        }
        // Ctrl-Q: yoink-side bail-out. Anything else (including Ctrl-C,
        // Ctrl-D, Ctrl-Z) is forwarded so it acts on the in-shell
        // process, not on yoink.
        if key.code == KeyCode::Char('q') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return true;
        }
        let Some(inner) = self.inner.as_ref() else {
            return false;
        };
        let Some(bytes) = encode_key(key) else {
            return false;
        };
        let stdin = inner.stdin.clone();
        let host = self.host.clone();
        let container = self.container.clone();
        tokio::spawn(async move {
            let mut guard = stdin.lock().await;
            if let Err(e) = guard.write_all(&bytes).await {
                warn!(host = %host.address, container = %container, error = %e, "exec stdin write failed");
            } else {
                let _ = guard.flush().await;
            }
            drop(ops);
        });
        false
    }

    pub fn render(&mut self, frame: &mut Frame<'_>) {
        let layout = pane_layout(frame.area());

        let header = Paragraph::new(format!(
            "yoink shell · {} · {}{}",
            self.host.address,
            self.container,
            if self.exited { " (exited)" } else { "" }
        ))
        .style(Style::default().add_modifier(ratatui::style::Modifier::BOLD));
        frame.render_widget(header, layout[0]);

        let body_area = layout[1];
        if let Some(err) = &self.error {
            let msg = Paragraph::new(err.as_str())
                .style(Style::default().fg(Color::Red))
                .block(Block::default().borders(Borders::ALL).title("shell"));
            frame.render_widget(msg, body_area);
        } else if let Some(inner) = self.inner.as_mut() {
            let block = Block::default().borders(Borders::ALL).title("shell");
            let pty = PseudoTerminal::new(inner.parser.screen()).block(block);
            frame.render_widget(pty, body_area);
        } else {
            let msg = Paragraph::new("(starting shell…)")
                .style(Style::default().fg(Color::DarkGray))
                .block(Block::default().borders(Borders::ALL).title("shell"));
            frame.render_widget(msg, body_area);
        }

        let footer = Paragraph::new("Ctrl-Q exit · Ctrl-D / `exit` end shell")
            .style(Style::default().fg(Color::DarkGray));
        frame.render_widget(footer, layout[2]);
    }

    /// Apply a new (rows, cols) to the parser and notify the daemon if
    /// dimensions actually changed. The app loop calls this each frame
    /// with the current inside-border size.
    pub fn apply_size(&mut self, ops: Arc<dyn DockerOps>, rows: u16, cols: u16) {
        let Some(inner) = self.inner.as_mut() else {
            return;
        };
        if rows == 0 || cols == 0 || (rows, cols) == inner.last_size {
            return;
        }
        inner.parser.screen_mut().set_size(rows, cols);
        inner.last_size = (rows, cols);
        // Skip the daemon-side resize until the exec is actually
        // started — otherwise bollard returns a 400 for the first
        // frame's apply_size call. The screen-side size is updated
        // either way so vt100 reflows immediately.
        if !inner.started {
            return;
        }
        let host = self.host.clone();
        let id = inner.id.clone();
        let kind = inner.kind;
        tokio::spawn(async move {
            let res = match kind {
                ExecKind::Exec => ops.resize_exec(&host, &id, rows, cols).await,
                ExecKind::Sidecar => ops.resize_container_tty(&host, &id, rows, cols).await,
            };
            if let Err(e) = res {
                warn!(host = %host.address, error = %e, "resize failed");
            }
        });
    }

    /// Force-remove the sidecar container (no-op for in-container
    /// exec). Called when the user presses Ctrl-Q so we don't leave a
    /// debug container running on the host. Fire-and-forget — Drop
    /// can't await, and best-effort is fine since `auto_remove` will
    /// reap it eventually anyway.
    pub fn cleanup(&mut self, ops: Arc<dyn DockerOps>) {
        let Some(inner) = self.inner.as_ref() else {
            return;
        };
        if inner.kind != ExecKind::Sidecar {
            return;
        }
        let host = self.host.clone();
        let name = inner.id.clone();
        tokio::spawn(async move {
            if let Err(e) = ops.force_remove_container(&host, &name).await {
                warn!(host = %host.address, container = %name, error = %e, "sidecar cleanup failed");
            }
        });
    }
}

/// Translate a crossterm `KeyEvent` into the byte sequence a PTY
/// expects. Covers the common cases (printable, enter, backspace,
/// arrows, function keys, Ctrl-letter); anything exotic is ignored
/// rather than guessed.
#[must_use]
pub fn encode_key(key: KeyEvent) -> Option<Vec<u8>> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let mut bytes: Vec<u8> = match key.code {
        KeyCode::Char(c) => {
            if ctrl {
                // Map Ctrl-A..Ctrl-Z to control bytes 0x01..0x1A. Other
                // Ctrl-printable combos (Ctrl-[, Ctrl-\\, etc.) fall
                // through to plain char encoding — terminals vary too
                // much to do better.
                let lc = c.to_ascii_lowercase();
                if lc.is_ascii_lowercase() {
                    vec![(lc as u8) - b'a' + 1]
                } else {
                    let mut buf = [0u8; 4];
                    c.encode_utf8(&mut buf).as_bytes().to_vec()
                }
            } else {
                let mut buf = [0u8; 4];
                c.encode_utf8(&mut buf).as_bytes().to_vec()
            }
        }
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Esc => vec![0x1b],
        KeyCode::Up => b"\x1b[A".to_vec(),
        KeyCode::Down => b"\x1b[B".to_vec(),
        KeyCode::Right => b"\x1b[C".to_vec(),
        KeyCode::Left => b"\x1b[D".to_vec(),
        KeyCode::Home => b"\x1b[H".to_vec(),
        KeyCode::End => b"\x1b[F".to_vec(),
        KeyCode::PageUp => b"\x1b[5~".to_vec(),
        KeyCode::PageDown => b"\x1b[6~".to_vec(),
        KeyCode::Delete => b"\x1b[3~".to_vec(),
        KeyCode::Insert => b"\x1b[2~".to_vec(),
        KeyCode::F(n) => match n {
            1 => b"\x1bOP".to_vec(),
            2 => b"\x1bOQ".to_vec(),
            3 => b"\x1bOR".to_vec(),
            4 => b"\x1bOS".to_vec(),
            5 => b"\x1b[15~".to_vec(),
            6 => b"\x1b[17~".to_vec(),
            7 => b"\x1b[18~".to_vec(),
            8 => b"\x1b[19~".to_vec(),
            9 => b"\x1b[20~".to_vec(),
            10 => b"\x1b[21~".to_vec(),
            11 => b"\x1b[23~".to_vec(),
            12 => b"\x1b[24~".to_vec(),
            _ => return None,
        },
        _ => return None,
    };
    if alt {
        // Standard alt-prefix: emit ESC then the key bytes. Most
        // terminals use this for Meta/Alt combos.
        bytes.insert(0, 0x1b);
    }
    Some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ke(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    #[test]
    fn encode_plain_char() {
        assert_eq!(encode_key(ke(KeyCode::Char('a'), KeyModifiers::NONE)).unwrap(), b"a");
    }

    #[test]
    fn encode_enter_is_cr() {
        assert_eq!(encode_key(ke(KeyCode::Enter, KeyModifiers::NONE)).unwrap(), b"\r");
    }

    #[test]
    fn encode_backspace_is_del() {
        assert_eq!(
            encode_key(ke(KeyCode::Backspace, KeyModifiers::NONE)).unwrap(),
            &[0x7f]
        );
    }

    #[test]
    fn encode_ctrl_c_is_etx() {
        assert_eq!(
            encode_key(ke(KeyCode::Char('c'), KeyModifiers::CONTROL)).unwrap(),
            &[0x03]
        );
    }

    #[test]
    fn encode_ctrl_d_is_eot() {
        assert_eq!(
            encode_key(ke(KeyCode::Char('d'), KeyModifiers::CONTROL)).unwrap(),
            &[0x04]
        );
    }

    #[test]
    fn encode_arrow_up() {
        assert_eq!(
            encode_key(ke(KeyCode::Up, KeyModifiers::NONE)).unwrap(),
            b"\x1b[A"
        );
    }

    #[test]
    fn encode_alt_char_prefixes_esc() {
        assert_eq!(
            encode_key(ke(KeyCode::Char('b'), KeyModifiers::ALT)).unwrap(),
            b"\x1bb"
        );
    }
}
