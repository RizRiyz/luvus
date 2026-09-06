//! PTY pane lifecycle. Unix panes use one poll-driven descriptor actor for
//! ordered input and output; Windows keeps portable-pty's split reader/writer
//! backend. Child waiting is one process-wide event-driven reaper.

use std::ffi::OsString;
#[cfg(windows)]
use std::io::Read;
#[cfg(any(windows, test))]
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
#[cfg(unix)]
use std::sync::OnceLock;
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::Result;
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};

use crate::event::AppEvent;
use crate::ids::PaneId;
use crate::terminal::appearance::PaneAppearance;
use crate::terminal::backend::TerminalRuntime;
use crate::terminal::vt::{create_engine, VtEngine, VtEngineKind};

mod io;
mod reaper;

/// Keep each pane's read working set small. Unix amortizes synchronization by
/// draining several of these chunks under one bounded engine lock.
const PTY_READ_BUFFER_BYTES: usize = 8 * 1024;

#[cfg(test)]
use reaper::child_poll_finished;
use reaper::register_child_reaper;
#[cfg(all(test, unix))]
use reaper::CHILD_REAPER_STARTS;

fn terminate_spawned_child(child: &mut (dyn portable_pty::Child + Send + Sync)) {
    let _ = child.kill();
    let _ = child.wait();
}

pub(crate) enum InputAction {
    Bytes(Vec<u8>),
    /// One protocol-visible submit operation. The paste and its Enter are one
    /// queue item, so accepting the action cannot report half a submission.
    Submit {
        paste: Vec<u8>,
        settle: std::time::Duration,
    },
}

/// Ordered PTY input plus an optional Unix actor wakeup. Terminal-generated
/// replies and user input share this sender, preserving their FIFO order.
#[derive(Clone)]
pub(crate) struct InputSender {
    sender: Sender<InputAction>,
    #[cfg(unix)]
    wake: Arc<OnceLock<Arc<io::unix_actor::WakePipe>>>,
}

impl InputSender {
    #[cfg(unix)]
    fn with_wake_slot(
        sender: Sender<InputAction>,
        wake: Arc<OnceLock<Arc<io::unix_actor::WakePipe>>>,
    ) -> Self {
        Self { sender, wake }
    }

    pub(crate) fn send(
        &self,
        action: InputAction,
    ) -> std::result::Result<(), mpsc::SendError<InputAction>> {
        self.sender.send(action)?;
        self.wake();
        Ok(())
    }

    fn wake(&self) {
        #[cfg(unix)]
        if let Some(wake) = self.wake.get() {
            wake.wake();
        }
    }
}

impl From<Sender<InputAction>> for InputSender {
    fn from(sender: Sender<InputAction>) -> Self {
        Self {
            sender,
            #[cfg(unix)]
            wake: Arc::new(OnceLock::new()),
        }
    }
}

#[cfg(any(windows, test))]
fn write_input_action(writer: &mut dyn Write, action: InputAction) -> std::io::Result<()> {
    match action {
        InputAction::Bytes(bytes) => writer.write_all(&bytes)?,
        InputAction::Submit { paste, settle } => {
            if !paste.is_empty() {
                writer.write_all(&paste)?;
                writer.flush()?;
                if !settle.is_zero() {
                    std::thread::sleep(settle);
                }
            }
            writer.write_all(b"\r")?;
        }
    }
    writer.flush()
}

/// A pane app's mouse-tracking state (all four DECSET-derived flags in one
/// read): whether it reports at all, whether it wants press-and-move (1002) or
/// any-motion (1003) events, and whether reports use the SGR encoding.
#[derive(Clone, Copy, Default)]
pub struct MouseModes {
    pub report: bool,
    pub drag: bool,
    pub motion: bool,
    pub sgr: bool,
    pub alternate_scroll: bool,
}

pub struct Pane {
    /// Stable application identity for operational lifecycle events. This is
    /// never derived from the child command or terminal contents.
    id: PaneId,
    pub engine: Arc<Mutex<dyn VtEngine>>,
    /// `None` until a deferred spawn's worker stores it (docs/82).
    master: Arc<Mutex<Option<Box<dyn MasterPty + Send>>>>,
    input_tx: InputSender,
    pub cwd: PathBuf,
    pub command: String,
    /// The shell's pid, for reading its live working directory and process
    /// tree. 0 means a deferred spawn has not finished yet — callers must
    /// treat that as "no process" rather than a real pid.
    pub child_pid: Arc<AtomicU32>,
    /// Stable metadata for this exact PTY lifetime. `None` while a deferred
    /// spawn is pending; assigned once after the PTY writer and root process are
    /// both ready, and never changed for moves or tab reordering.
    terminal_runtime: Arc<Mutex<Option<TerminalRuntime>>>,
    /// Monotonic content generation for this PTY lifetime. The reader advances
    /// it while holding the VT lock, so capture can return a revision that
    /// exactly matches the screen snapshot it serialized.
    content_revision: Arc<AtomicU64>,
    /// `PtyData` coalescing: set by the reader when it announces new output,
    /// cleared by the app loop when it consumes the event. While set, further
    /// reads skip the send — a saturated PTY (thousands of 8 KB reads/s) wakes
    /// the loop once per iteration instead of once per read (measured ~30% of
    /// a core of pure wakeup churn during a `yes` firehose).
    data_pending: Arc<AtomicBool>,
    /// Set by the reaper once the child has been waited on. After that the OS may
    /// recycle its pid, so [`Drop`] must never signal it — it could hit an
    /// unrelated process.
    child_exited: Arc<AtomicBool>,
    /// The latest requested size, shared with the deferred spawn worker so a
    /// resize racing the spawn still lands.
    size: Arc<Mutex<(u16, u16)>>,
    /// Set by `Drop` so a close-before-spawn aborts the spawn worker.
    cancelled: Arc<AtomicBool>,
}

impl Drop for Pane {
    /// Hang up the child, exactly like closing a terminal window.
    ///
    /// Without this the pane leaks everything it owns. Dropping `master` closes
    /// only luvus's own handle: the reader thread still holds a cloned PTY fd, so
    /// the child never sees EOF, so the reader never returns, so it keeps the
    /// engine `Arc` (and its whole scrollback grid) alive forever — and the
    /// writer thread with it, since the engine holds a clone of `input_tx`.
    /// Measured at 8 panes opened and closed: 8 orphaned shells and ~170 MB never
    /// reclaimed. It matters because the server deliberately outlives its windows,
    /// so a long session accumulates one leak per closed pane.
    ///
    /// Killing the child breaks the cycle: the reader hits EOF and exits, which
    /// releases the engine and the scrollback, and the last `input_tx` drop ends
    /// the writer.
    fn drop(&mut self) {
        // A deferred spawn that hasn't forked yet aborts in the worker; one
        // that has forked is killed by the worker's own post-fork check.
        self.cancelled.store(true, Ordering::SeqCst);
        self.input_tx.wake();
        // Already reaped → the pid may belong to someone else now.
        if self.child_exited.load(Ordering::SeqCst) {
            return;
        }
        let pid = self.child_pid.load(Ordering::SeqCst);
        if pid == 0 {
            return;
        }
        // SIGHUP rather than SIGKILL: a shell hangs up its jobs and exits
        // cleanly, and a deliberately `nohup`ed process still survives — the
        // same contract as closing the terminal window.
        #[cfg(unix)]
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGHUP);
        }
        // No signals on Windows; end the whole child tree instead.
        #[cfg(windows)]
        {
            let _ = crate::platform::no_window(
                std::process::Command::new("taskkill")
                    .args(["/PID", &pid.to_string(), "/T", "/F"])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null()),
            )
            .spawn();
        }
    }
}

impl Pane {
    /// Spawn an interactive shell pane.
    #[allow(clippy::too_many_arguments)]
    pub fn spawn(
        id: PaneId,
        cols: u16,
        rows: u16,
        cwd: PathBuf,
        app_tx: Sender<AppEvent>,
        initial: Option<&str>,
        shell: &str,
        history_budget_bytes: usize,
        appearance: PaneAppearance,
    ) -> Result<Pane> {
        let cmd = CommandBuilder::new(shell);
        Self::build(
            id,
            cols,
            rows,
            cwd,
            app_tx,
            initial,
            cmd,
            basename(shell),
            &[],
            history_budget_bytes,
            appearance,
        )
    }

    /// Spawn a shell pane whose shell starts by running a command (built by
    /// `platform::shell_run_then_interactive`) — a restored agent pane resumes
    /// its session on launch, no resume command typed at a visible prompt. The
    /// pane keeps the shell's label so snapshots stay consistent with `spawn`.
    #[allow(clippy::too_many_arguments)]
    pub fn spawn_shell_with(
        id: PaneId,
        cols: u16,
        rows: u16,
        cwd: PathBuf,
        app_tx: Sender<AppEvent>,
        initial: Option<&str>,
        shell: &str,
        argv: &[String],
        history_budget_bytes: usize,
        appearance: PaneAppearance,
    ) -> Result<Pane> {
        let Some((program, args)) = argv.split_first() else {
            return Err(anyhow::anyhow!("empty shell command"));
        };
        let mut cmd = CommandBuilder::new(program);
        for a in args {
            cmd.arg(a);
        }
        Self::build(
            id,
            cols,
            rows,
            cwd,
            app_tx,
            initial,
            cmd,
            basename(shell),
            &[],
            history_budget_bytes,
            appearance,
        )
    }

    /// Spawn a pane running an explicit argv with extra environment — a module
    /// pane (docs/13 MOD-2). luvus's own identity vars always win over `env`.
    #[allow(clippy::too_many_arguments)]
    pub fn spawn_command(
        id: PaneId,
        cols: u16,
        rows: u16,
        cwd: PathBuf,
        app_tx: Sender<AppEvent>,
        argv: &[String],
        env: &[(String, String)],
        history_budget_bytes: usize,
        appearance: PaneAppearance,
    ) -> Result<Pane> {
        let Some((program, args)) = argv.split_first() else {
            return Err(anyhow::anyhow!("empty module command"));
        };
        let mut cmd = CommandBuilder::new(program);
        for a in args {
            cmd.arg(a);
        }
        Self::build(
            id,
            cols,
            rows,
            cwd,
            app_tx,
            None,
            cmd,
            basename(program),
            env,
            history_budget_bytes,
            appearance,
        )
    }

    /// Spawn an interactive shell pane whose child process starts **after**
    /// this returns (docs/82): the pane exists (engine, input queue, writer)
    /// immediately, and a worker thread opens the PTY, forks the shell, and
    /// hands the writer over. `pane split` answers the CLI without paying the
    /// fork cost on the loop thread.
    #[allow(clippy::too_many_arguments)]
    pub fn spawn_deferred(
        id: PaneId,
        cols: u16,
        rows: u16,
        cwd: PathBuf,
        fallback_cwds: &[PathBuf],
        app_tx: Sender<AppEvent>,
        shell: &str,
        history_budget_bytes: usize,
        appearance: PaneAppearance,
    ) -> Pane {
        let cmd = CommandBuilder::new(shell);
        Self::build_deferred(
            id,
            cols,
            rows,
            cwd,
            fallback_cwds,
            app_tx,
            None,
            cmd,
            basename(shell),
            &[],
            history_budget_bytes,
            appearance,
        )
    }

    /// Restore an interactive shell pane without making session loading wait
    /// for the operating system to allocate its PTY. The saved screen is
    /// replayed before this returns, so clients can render useful content while
    /// the shell starts in the background.
    #[allow(clippy::too_many_arguments)]
    pub fn spawn_restored(
        id: PaneId,
        cols: u16,
        rows: u16,
        cwd: PathBuf,
        fallback_cwds: &[PathBuf],
        app_tx: Sender<AppEvent>,
        initial: Option<&str>,
        shell: &str,
        history_budget_bytes: usize,
        appearance: PaneAppearance,
    ) -> Pane {
        let cmd = CommandBuilder::new(shell);
        Self::build_deferred(
            id,
            cols,
            rows,
            cwd,
            fallback_cwds,
            app_tx,
            initial,
            cmd,
            basename(shell),
            &[],
            history_budget_bytes,
            appearance,
        )
    }

    /// Deferred counterpart to [`Pane::spawn_shell_with`] for restoring a
    /// PowerShell agent session without blocking server startup.
    #[allow(clippy::too_many_arguments)]
    pub fn spawn_shell_with_deferred(
        id: PaneId,
        cols: u16,
        rows: u16,
        cwd: PathBuf,
        fallback_cwds: &[PathBuf],
        app_tx: Sender<AppEvent>,
        initial: Option<&str>,
        shell: &str,
        argv: &[String],
        history_budget_bytes: usize,
        appearance: PaneAppearance,
    ) -> Result<Pane> {
        let Some((program, args)) = argv.split_first() else {
            return Err(anyhow::anyhow!("empty shell command"));
        };
        let mut cmd = CommandBuilder::new(program);
        for arg in args {
            cmd.arg(arg);
        }
        Ok(Self::build_deferred(
            id,
            cols,
            rows,
            cwd,
            fallback_cwds,
            app_tx,
            initial,
            cmd,
            basename(shell),
            &[],
            history_budget_bytes,
            appearance,
        ))
    }

    #[allow(clippy::too_many_arguments)]
    fn build(
        id: PaneId,
        cols: u16,
        rows: u16,
        cwd: PathBuf,
        app_tx: Sender<AppEvent>,
        initial: Option<&str>,
        mut cmd: CommandBuilder,
        command: String,
        extra_env: &[(String, String)],
        history_budget_bytes: usize,
        appearance: PaneAppearance,
    ) -> Result<Pane> {
        let pty_system = native_pty_system();
        let pair = pty_system.openpty(PtySize {
            rows: rows.max(1),
            cols: cols.max(1),
            pixel_width: 0,
            pixel_height: 0,
        })?;

        apply_pane_env(&mut cmd, id, &cwd, extra_env);
        let mut child = pair.slave.spawn_command(cmd)?;
        let child_pid = child
            .process_id()
            .expect("a spawned child always has a pid");
        let terminal_runtime = match TerminalRuntime::new(child_pid) {
            Ok(runtime) => runtime,
            Err(error) => {
                terminate_spawned_child(child.as_mut());
                return Err(anyhow::Error::msg(error));
            }
        };
        drop(pair.slave);

        // User input and terminal-generated responses share one ordered queue.
        // Unix wakes one poll-driven actor; Windows retains the split backend.
        let (input_tx, input_rx) = io::input_channel();
        let engine = create_engine(
            VtEngineKind::default(),
            cols,
            rows,
            input_tx.clone(),
            history_budget_bytes,
            appearance,
        );
        // Replay the saved screen so a restored pane shows its prior content.
        if let Some(screen) = initial {
            if let Ok(mut e) = engine.lock() {
                e.advance(screen.as_bytes());
            }
        }

        let data_pending = Arc::new(AtomicBool::new(false));
        let content_revision = Arc::new(AtomicU64::new(0));
        let cancelled = Arc::new(AtomicBool::new(false));
        if let Err(error) = io::start(
            id,
            pair.master.as_ref(),
            input_rx,
            engine.clone(),
            app_tx.clone(),
            data_pending.clone(),
            content_revision.clone(),
            cancelled.clone(),
        ) {
            terminate_spawned_child(child.as_mut());
            return Err(error.into());
        }

        // Reap the child so we notice it exiting. The shared reaper sets the
        // exit flag *before* the event goes out, so by the time the loop closes
        // the pane (and drops it) `Drop` already knows not to signal a
        // possibly-recycled pid.
        let child_exited = Arc::new(AtomicBool::new(false));
        register_child_reaper(id, child, child_exited.clone(), app_tx);

        Ok(Pane {
            id,
            engine,
            child_pid: Arc::new(AtomicU32::new(child_pid)),
            terminal_runtime: Arc::new(Mutex::new(Some(terminal_runtime))),
            content_revision,
            master: Arc::new(Mutex::new(Some(pair.master))),
            input_tx,
            cwd,
            command,
            data_pending,
            child_exited,
            size: Arc::new(Mutex::new((cols, rows))),
            cancelled,
        })
    }

    /// The deferred half of `spawn_deferred` (docs/82). Returns a fully usable
    /// pane immediately; the fork happens on a worker thread. Failures there
    /// surface as `AppEvent::PtyExit(id)`, exactly like a child that died at
    /// startup, so no pane is left silently dead.
    #[allow(clippy::too_many_arguments)]
    fn build_deferred(
        id: PaneId,
        cols: u16,
        rows: u16,
        cwd: PathBuf,
        fallback_cwds: &[PathBuf],
        app_tx: Sender<AppEvent>,
        initial: Option<&str>,
        cmd: CommandBuilder,
        command: String,
        extra_env: &[(String, String)],
        history_budget_bytes: usize,
        appearance: PaneAppearance,
    ) -> Pane {
        // Everything a caller can observe before the child exists: the engine
        // (pane.read, detection, rendering) and the input queue.
        let (input_tx, input_rx) = io::input_channel();
        let engine = create_engine(
            VtEngineKind::default(),
            cols,
            rows,
            input_tx.clone(),
            history_budget_bytes,
            appearance,
        );
        if let Some(screen) = initial {
            if let Ok(mut engine) = engine.lock() {
                engine.advance(screen.as_bytes());
            }
        }

        let child_pid = Arc::new(AtomicU32::new(0));
        let terminal_runtime: Arc<Mutex<Option<TerminalRuntime>>> = Arc::new(Mutex::new(None));
        let master: Arc<Mutex<Option<Box<dyn MasterPty + Send>>>> = Arc::new(Mutex::new(None));
        let data_pending = Arc::new(AtomicBool::new(false));
        let content_revision = Arc::new(AtomicU64::new(0));
        let child_exited = Arc::new(AtomicBool::new(false));
        let size = Arc::new(Mutex::new((cols, rows)));
        let cancelled = Arc::new(AtomicBool::new(false));

        let worker = {
            let (
                engine,
                child_pid,
                terminal_runtime,
                master,
                data_pending,
                content_revision,
                child_exited,
                size,
                cancelled,
            ) = (
                engine.clone(),
                child_pid.clone(),
                terminal_runtime.clone(),
                master.clone(),
                data_pending.clone(),
                content_revision.clone(),
                child_exited.clone(),
                size.clone(),
                cancelled.clone(),
            );
            let tx = app_tx.clone();
            // Owned copies for the 'static worker thread.
            let worker_cwd = cwd.clone();
            let worker_fallback_cwds = fallback_cwds.to_vec();
            let worker_env = extra_env.to_vec();
            thread::spawn(move || {
                let fail = || {
                    let _ = tx.send(AppEvent::PtyExit(id));
                };

                let (cols, rows) = *size.lock().unwrap_or_else(|p| p.into_inner());
                let pty_system = native_pty_system();
                let pair = match pty_system.openpty(PtySize {
                    rows: rows.max(1),
                    cols: cols.max(1),
                    pixel_width: 0,
                    pixel_height: 0,
                }) {
                    Ok(pair) => pair,
                    Err(_) => return fail(),
                };
                // Closed before the fork: abort without creating the child.
                if cancelled.load(Ordering::SeqCst) {
                    return;
                }
                let mut spawned = None;
                for candidate in std::iter::once(worker_cwd)
                    .chain(worker_fallback_cwds)
                    .filter(|candidate| candidate.is_dir())
                {
                    let mut candidate_cmd = cmd.clone();
                    apply_pane_env(&mut candidate_cmd, id, &candidate, &worker_env);
                    if let Ok(child) = pair.slave.spawn_command(candidate_cmd) {
                        spawned = Some((child, candidate));
                        break;
                    }
                }
                let Some((mut child, spawned_cwd)) = spawned else {
                    return fail();
                };
                let Some(pid) = child.process_id() else {
                    terminate_spawned_child(child.as_mut());
                    return fail();
                };
                let runtime = match TerminalRuntime::new(pid) {
                    Ok(runtime) => runtime,
                    Err(_) => {
                        terminate_spawned_child(child.as_mut());
                        return fail();
                    }
                };
                drop(pair.slave);
                // Publish the pid *before* the cancellation check: a concurrent
                // `Drop` then either sees the pid and signals it, or sees 0 — in
                // which case its `cancelled` store precedes this load and the
                // hangup below runs. Checking first would let `Drop` read 0,
                // return, and leave the child running with no owner.
                child_pid.store(pid, Ordering::SeqCst);
                // Closed between fork and handoff: hang up the fresh child and
                // reap it ourselves, since no reaper thread will be spawned.
                if cancelled.load(Ordering::SeqCst) {
                    #[cfg(unix)]
                    unsafe {
                        libc::kill(pid as libc::pid_t, libc::SIGHUP);
                    }
                    #[cfg(windows)]
                    {
                        let _ = crate::platform::no_window(
                            std::process::Command::new("taskkill")
                                .args(["/PID", &pid.to_string(), "/T", "/F"])
                                .stdout(std::process::Stdio::null())
                                .stderr(std::process::Stdio::null()),
                        )
                        .spawn();
                    }
                    let mut child = child;
                    let _ = child.wait();
                    child_exited.store(true, Ordering::SeqCst);
                    return;
                }

                // A resize raced the spawn: re-apply the latest size.
                let latest = *size.lock().unwrap_or_else(|p| p.into_inner());
                if latest != (cols, rows) {
                    let _ = pair.master.resize(PtySize {
                        rows: latest.1.max(1),
                        cols: latest.0.max(1),
                        pixel_width: 0,
                        pixel_height: 0,
                    });
                }

                if io::start(
                    id,
                    pair.master.as_ref(),
                    input_rx,
                    engine,
                    tx.clone(),
                    data_pending,
                    content_revision,
                    cancelled.clone(),
                )
                .is_err()
                {
                    terminate_spawned_child(child.as_mut());
                    child_exited.store(true, Ordering::SeqCst);
                    return fail();
                }
                *master.lock().unwrap_or_else(|p| p.into_inner()) = Some(pair.master);
                let ready_tx = tx.clone();
                register_child_reaper(id, child, child_exited, tx.clone());
                *terminal_runtime
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(runtime);
                let _ = ready_tx.send(AppEvent::PtyReady {
                    id,
                    cwd: spawned_cwd,
                });
            })
        };
        drop(worker);

        Pane {
            id,
            engine,
            child_pid,
            terminal_runtime,
            content_revision,
            master,
            input_tx,
            cwd,
            command,
            data_pending,
            child_exited,
            size,
            cancelled,
        }
    }

    /// Consume the pending-output flag (the loop's re-arm cadence). Returns
    /// whether it was set — i.e. output arrived since the last re-arm, so the
    /// caller may owe one more render for the tail of a burst.
    pub fn take_data_pending(&self) -> bool {
        let pending = self
            .data_pending
            .swap(false, std::sync::atomic::Ordering::AcqRel);
        // Unix compacts on a bounded deadline in its existing descriptor
        // actor. Doing it here would repeatedly pack and re-inflate rows while
        // one large output stream is still being consumed. The blocking
        // Windows reader has no such event-loop deadline, so retain its
        // coalesced app-boundary maintenance.
        #[cfg(windows)]
        if pending {
            if let Ok(mut engine) = self.engine.lock() {
                engine.finish_output_batch();
            }
        }
        pending
    }

    /// Observe whether output is waiting for a coalescing boundary without
    /// consuming it. The server uses this to arm the 100 ms fallback only while
    /// a pane actually has pending bytes, instead of waking forever when idle.
    pub fn has_data_pending(&self) -> bool {
        self.data_pending.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Clear the pending-output coalescing flag so the reader's next
    /// announcement fires immediately. Input is interaction, not background
    /// output, and a parked `wait.output` must stay event-driven (docs/81).
    pub fn rearm_pty_notify(&self) {
        self.data_pending
            .store(false, std::sync::atomic::Ordering::Release);
    }

    pub fn send(&self, bytes: &[u8]) {
        self.rearm_pty_notify();
        let _ = self.input_tx.send(InputAction::Bytes(bytes.to_vec()));
    }

    /// Enqueue input and report a closed PTY writer instead of silently
    /// treating it as delivery. Review-note handoff uses this before recording
    /// delivery metadata; ordinary interactive input keeps the infallible API.
    pub fn try_send(&self, bytes: &[u8]) -> Result<(), String> {
        self.rearm_pty_notify();
        self.input_tx
            .send(InputAction::Bytes(bytes.to_vec()))
            .map_err(|_| "target pane closed before input was delivered".to_string())
    }

    #[cfg(test)]
    pub(crate) fn replace_input_sender_for_test(&mut self, sender: Sender<InputAction>) {
        self.input_tx = sender.into();
    }

    /// Enqueue one atomic submitted-text action for protocol consumers. Queue
    /// success is dispatch evidence only; it does not claim the child consumed
    /// or acted on the bytes.
    pub fn try_submit_text(&self, text: &str) -> Result<(), String> {
        let bracketed = self
            .engine
            .lock()
            .map(|engine| engine.bracketed_paste())
            .unwrap_or(false);
        self.rearm_pty_notify();
        self.input_tx
            .send(InputAction::Submit {
                paste: if text.is_empty() {
                    Vec::new()
                } else {
                    wrap_paste(text, bracketed)
                },
                settle: std::time::Duration::from_millis(30),
            })
            .map_err(|_| "target pane closed before input was queued".to_string())
    }

    pub fn terminal_runtime(&self) -> Option<TerminalRuntime> {
        self.terminal_runtime
            .lock()
            .ok()
            .and_then(|runtime| runtime.clone())
    }

    pub fn content_revision(&self) -> u64 {
        self.content_revision.load(Ordering::Acquire)
    }

    pub fn content_revision_handle(&self) -> Arc<AtomicU64> {
        self.content_revision.clone()
    }

    pub fn child_exited(&self) -> bool {
        self.child_exited.load(Ordering::SeqCst)
    }

    /// Enqueue `bytes` after `delay`, off-thread. Used to follow a pasted prompt
    /// with a submit key once the child has ingested the paste: an agent's input
    /// widget needs the paste to land before the Enter, or the Enter is swallowed
    /// into the paste. The cloned input channel keeps the writer alive for exactly
    /// this one deferred send.
    pub fn send_after(&self, bytes: Vec<u8>, delay: std::time::Duration) {
        let tx = self.input_tx.clone();
        std::thread::spawn(move || {
            std::thread::sleep(delay);
            let _ = tx.send(InputAction::Bytes(bytes));
        });
    }

    /// Apply a new per-pane history memory budget (Settings → Layout). Shrinks
    /// retained history immediately when lowered.
    pub fn set_history_budget(&self, bytes: usize) {
        if let Ok(mut e) = self.engine.lock() {
            e.set_history_budget(bytes);
        }
    }

    /// Scroll this pane's scrollback viewport `delta` lines (positive = up into
    /// history). No-op on the alternate screen (the running app owns scrolling).
    pub fn scroll(&self, delta: i32) {
        if let Ok(mut e) = self.engine.lock() {
            e.scroll(delta);
        }
    }

    /// Jump the viewport to the top of retained scrollback.
    pub fn scroll_to_top(&self) {
        if let Ok(mut e) = self.engine.lock() {
            e.scroll_to_top();
        }
    }

    /// Snap the viewport back to the live bottom.
    pub fn scroll_to_bottom(&self) {
        if let Ok(mut e) = self.engine.lock() {
            e.scroll_to_bottom();
        }
    }

    /// Number of retained rows, including the visible screen.
    pub fn retained_row_count(&self) -> usize {
        self.engine
            .lock()
            .map(|e| e.retained_row_count())
            .unwrap_or(0)
    }

    /// `(visible_top, retained_rows)` from one terminal snapshot. Mouse
    /// selection uses this to bind a screen row to retained history without an
    /// output burst changing the history length between separate reads.
    pub(crate) fn retained_viewport(&self) -> Option<(usize, usize)> {
        self.engine.lock().ok().map(|engine| {
            (
                engine.history_len().saturating_sub(engine.scroll_offset()),
                engine.retained_row_count(),
            )
        })
    }

    /// Read one retained row without allocating every other row.
    #[cfg(test)]
    pub fn retained_row_text(&self, index: usize) -> Option<String> {
        self.engine.lock().ok()?.retained_row_text(index)
    }

    /// Visit retained rows under one engine lock. Each callback receives the
    /// row index, history length, total row count, and text from one consistent
    /// terminal snapshot.
    pub fn for_each_retained_row(&self, f: &mut dyn FnMut(usize, usize, usize, &str)) {
        if let Ok(engine) = self.engine.lock() {
            let history = engine.history_len();
            let row_count = engine.retained_row_count();
            engine.for_each_retained_row(&mut |index, line| f(index, history, row_count, line));
        }
    }

    /// Extract retained terminal text by grid cell rather than string index.
    /// This keeps wide glyphs and combining sequences aligned with selection
    /// highlights.
    pub fn retained_selection_text(
        &self,
        range: ((usize, usize), (usize, usize)),
    ) -> Option<String> {
        self.engine.lock().ok()?.retained_selection_text(range)
    }

    /// Extract a selection expressed in the currently visible viewport. This
    /// is the fallback when a mouse press could not snapshot retained-history
    /// coordinates, and still preserves terminal cell semantics for Unicode.
    pub fn visible_selection_text(
        &self,
        ((start_row, start_col), (end_row, end_col)): ((usize, usize), (usize, usize)),
    ) -> Option<String> {
        let engine = self.engine.lock().ok()?;
        let visible_top = engine.history_len().saturating_sub(engine.scroll_offset());
        let row_count = engine.retained_row_count();
        let last_row = row_count.checked_sub(1)?;
        engine.retained_selection_text((
            (
                visible_top.saturating_add(start_row).min(last_row),
                start_col,
            ),
            (visible_top.saturating_add(end_row).min(last_row), end_col),
        ))
    }

    /// Cell geometry used by keyboard copy-mode navigation.
    pub fn retained_row_layout(
        &self,
        index: usize,
    ) -> Option<crate::terminal::vt::RetainedRowLayout> {
        self.engine.lock().ok()?.retained_row_layout(index)
    }

    /// Jump the scrollback viewport so the row `offset` lines above the live
    /// bottom is at the top, to land on a search match (docs/63).
    pub fn scroll_to(&self, offset: usize) {
        if let Ok(mut e) = self.engine.lock() {
            e.scroll_to(offset);
        }
    }

    /// `(offset, history_len)` — the current scroll position and the total
    /// scrollback, read together under one lock (for scroll mode's `1`–`9` jump).
    pub fn scroll_state(&self) -> (usize, usize) {
        self.engine
            .lock()
            .map(|e| (e.scroll_offset(), e.history_len()))
            .unwrap_or((0, 0))
    }

    /// Read-only accounting for retained terminal history.
    pub fn history_metrics(&self) -> crate::terminal::vt::HistoryMetrics {
        self.engine.lock().map(|e| e.history_metrics()).unwrap_or(
            crate::terminal::vt::HistoryMetrics {
                offset: 0,
                retained_rows: 0,
                budget_bytes: 0,
                retained_bytes: 0,
                estimated_grid_bytes: 0,
                cache_bytes: None,
                compacted_rows: None,
                allocated_cells: None,
                packed_blocks: None,
                packed_bytes: None,
                packed_rows: None,
                dense_row_bytes: None,
                row_descriptor_bytes: None,
                allocation_count: None,
                exact_bytes: false,
            },
        )
    }

    /// Whether the child is on the alternate screen — callers forward wheel
    /// input to the app there instead of scrolling scrollback.
    pub fn alt_screen(&self) -> bool {
        self.engine.lock().map(|e| e.alt_screen()).unwrap_or(false)
    }

    /// Whether the child enabled application cursor mode (DECCKM). When it has,
    /// the key encoder sends SS3 (`ESC O` + letter) cursor-key codes so apps
    /// like `less` that are strict about the mode recognize the keys.
    pub fn application_cursor(&self) -> bool {
        self.engine
            .lock()
            .map(|e| e.application_cursor())
            .unwrap_or(false)
    }

    /// Input modes read together under one engine lock.
    pub fn key_encoding_modes(&self) -> (bool, bool) {
        self.engine
            .lock()
            .map(|e| (e.application_cursor(), e.disambiguate_escape_codes()))
            .unwrap_or((false, false))
    }

    /// `(mouse_report, sgr)` — whether the child tracks the mouse, and whether
    /// it wants SGR-encoded reports. Read together under one lock.
    /// The app's mouse-tracking state, read under **one** engine lock — callers
    /// cache what they need (e.g. for the length of a drag) rather than re-lock
    /// per event, since the PTY reader holds this mutex during output bursts.
    pub fn mouse_mode(&self) -> MouseModes {
        self.engine
            .lock()
            .map(|e| MouseModes {
                report: e.mouse_report(),
                drag: e.mouse_drag(),
                motion: e.mouse_motion(),
                sgr: e.sgr_mouse(),
                alternate_scroll: e.alternate_scroll(),
            })
            .unwrap_or_default()
    }

    /// Whether plain page keys should scroll Luvus's host history. Full-screen
    /// programs, mouse-tracking TUIs, and primary-screen pagers in application
    /// cursor mode retain their own key handling.
    pub fn host_page_keys(&self) -> bool {
        self.engine
            .lock()
            .map(|e| !e.alt_screen() && !e.mouse_report() && !e.application_cursor())
            .unwrap_or(false)
    }

    /// Send pasted text to the child, wrapped in the bracketed-paste markers
    /// when the child asked for them (DECSET 2004).
    ///
    /// The outer terminal hands luvus a paste with its markers already stripped
    /// (crossterm turns `ESC[200~ … ESC[201~` into one `Event::Paste`), so
    /// forwarding the bare text would make the child see it as ordinary typing.
    /// Programs that distinguish the two then misbehave: an agent CLI shows a
    /// dropped file's path as literal text instead of attaching the file, and
    /// vim auto-indents pasted code. Re-wrapping restores the distinction.
    pub fn send_paste(&self, text: &str) {
        let bracketed = self
            .engine
            .lock()
            .map(|e| e.bracketed_paste())
            .unwrap_or(false);
        self.send(&wrap_paste(text, bracketed));
    }

    pub fn try_send_paste(&self, text: &str) -> Result<(), String> {
        let bracketed = self
            .engine
            .lock()
            .map(|engine| engine.bracketed_paste())
            .unwrap_or(false);
        self.try_send(&wrap_paste(text, bracketed))
    }

    /// Resize the PTY + engine. Returns whether the size actually changed (so the
    /// caller can note the resize for detection's post-resize grace, docs/07).
    /// A deferred pane that has not spawned yet records the size; the spawn
    /// worker applies it (docs/82).
    pub fn resize(&mut self, cols: u16, rows: u16) -> bool {
        if cols == 0 || rows == 0 {
            return false;
        }
        {
            let mut size = self.size.lock().unwrap_or_else(|p| p.into_inner());
            if (cols, rows) == *size {
                return false;
            }
            *size = (cols, rows);
        }
        if let Ok(master) = self.master.lock() {
            if let Some(master) = master.as_ref() {
                let _ = master.resize(PtySize {
                    rows,
                    cols,
                    pixel_width: 0,
                    pixel_height: 0,
                });
            }
        }
        if let Ok(mut e) = self.engine.lock() {
            e.resize(cols, rows);
        }
        crate::logging::event(
            crate::logging::EventKind::PtyResize,
            &[
                crate::logging::Field::PaneId(u64::from(self.id.0)),
                crate::logging::Field::Cols(u64::from(cols)),
                crate::logging::Field::Rows(u64::from(rows)),
            ],
        );
        true
    }

    #[cfg(test)]
    pub(crate) fn size(&self) -> (u16, u16) {
        *self.size.lock().unwrap_or_else(|p| p.into_inner())
    }
}

/// Pasted text as the bytes to write to the child: wrapped in the
/// bracketed-paste markers when `bracketed`, bare otherwise.
fn wrap_paste(text: &str, bracketed: bool) -> Vec<u8> {
    if !bracketed {
        return text.as_bytes().to_vec();
    }
    let mut out = Vec::with_capacity(text.len() + 12);
    out.extend_from_slice(b"\x1b[200~");
    out.extend_from_slice(text.as_bytes());
    out.extend_from_slice(b"\x1b[201~");
    out
}

/// The file-name component of a program path, for the pane's display command.
fn basename(s: &str) -> String {
    std::path::Path::new(s)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(s)
        .to_string()
}

/// Apply the working directory and identity environment every pane child
/// inherits — shared by the synchronous and deferred spawn paths so a pane
/// always gets the same contract regardless of when its shell starts.
fn apply_pane_env(
    cmd: &mut CommandBuilder,
    id: PaneId,
    cwd: &std::path::Path,
    extra_env: &[(String, String)],
) {
    cmd.cwd(cwd);
    // Caller-supplied env first, then luvus's identity vars (so they can't
    // be overridden — no spoofing the module/pane identity).
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    cmd.env("TERM", "xterm-256color");
    cmd.env("LUVUS_ENV", "1");
    cmd.env("LUVUS_PANE_ID", id.0.to_string());
    if let Some(sock) = crate::ipc::api::socket_path_env() {
        cmd.env("LUVUS_SOCKET_PATH", &sock);
    }
    if let Some(address) = crate::ipc::api::socket_address_env() {
        cmd.env("LUVUS_API_ADDRESS", &address);
    }
    if let Some(name) = crate::session::active_name() {
        cmd.env(crate::session::SESSION_ENV_VAR, &name);
    }
    // This session's exact binary, so an agent can use `$LUVUS_BIN_PATH`
    // instead of a `luvus` on PATH that may be an older install with a
    // different CLI (skill/binary skew). Matches the server it talks to.
    if let Ok(exe) = std::env::current_exe() {
        cmd.env("LUVUS_BIN_PATH", &exe);
        if let Some(path) = path_with_server_binary(&exe, std::env::var_os("PATH")) {
            cmd.env("PATH", path);
        }
    }
}

/// Put the server's own executable directory first without dropping the
/// user's existing command search path. A debug server therefore gives its
/// panes the debug CLI, while an installed server gives them that exact
/// release CLI. `split_paths`/`join_paths` keep this portable across Unix and
/// Windows and avoid shell-specific quoting.
fn path_with_server_binary(exe: &Path, inherited: Option<OsString>) -> Option<OsString> {
    let binary_dir = exe.parent()?;
    let mut entries = vec![binary_dir.to_path_buf()];
    if let Some(inherited) = inherited {
        entries.extend(
            std::env::split_paths(&inherited).filter(|entry| entry.as_path() != binary_dir),
        );
    }
    std::env::join_paths(entries).ok()
}

#[cfg(windows)]
fn read_loop(
    id: PaneId,
    mut reader: Box<dyn Read + Send>,
    engine: Arc<Mutex<dyn VtEngine>>,
    tx: Sender<AppEvent>,
    data_pending: Arc<AtomicBool>,
    content_revision: Arc<AtomicU64>,
) {
    let mut buf = [0u8; PTY_READ_BUFFER_BYTES];
    loop {
        match reader.read(&mut buf) {
            Ok(0) | Err(_) => {
                let _ = tx.send(AppEvent::PtyExit(id));
                break;
            }
            Ok(n) => {
                if let Ok(mut e) = engine.lock() {
                    e.advance(&buf[..n]);
                    content_revision.fetch_add(1, Ordering::Release);
                }
                // Announce new output only when no announcement is already in
                // flight — the loop reads the engine's *latest* state anyway,
                // so a burst needs one wakeup, not one per read.
                if !data_pending.swap(true, Ordering::AcqRel)
                    && tx.send(AppEvent::PtyData(id)).is_err()
                {
                    break;
                }
            }
        }
    }
}

#[cfg(all(test, unix))]
mod reap_tests {
    use super::*;
    use crate::ids::PaneId;

    struct SignalMaskGuard(libc::sigset_t);

    impl SignalMaskGuard {
        fn block_sigchld() -> Self {
            unsafe {
                let mut sigchld: libc::sigset_t = std::mem::zeroed();
                libc::sigemptyset(&mut sigchld);
                libc::sigaddset(&mut sigchld, libc::SIGCHLD);
                let mut previous: libc::sigset_t = std::mem::zeroed();
                let result = libc::pthread_sigmask(libc::SIG_BLOCK, &sigchld, &mut previous);
                assert_eq!(
                    result,
                    0,
                    "block SIGCHLD: {}",
                    std::io::Error::from_raw_os_error(result)
                );
                Self(previous)
            }
        }
    }

    impl Drop for SignalMaskGuard {
        fn drop(&mut self) {
            unsafe {
                let _ = libc::pthread_sigmask(libc::SIG_SETMASK, &self.0, std::ptr::null_mut());
            }
        }
    }

    fn alive(pid: u32) -> bool {
        unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
    }

    fn wait_gone(pid: u32) -> bool {
        for _ in 0..40 {
            if !alive(pid) {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        false
    }

    fn spawn_sh() -> Pane {
        let (tx, rx) = mpsc::channel();
        std::mem::forget(rx); // the real app holds the receiver for the session
        Pane::spawn(
            PaneId::alloc(),
            80,
            24,
            std::env::temp_dir(),
            tx,
            None,
            "/bin/sh",
            500,
            PaneAppearance::default(),
        )
        .expect("spawn")
    }

    #[test]
    fn panes_share_one_child_reaper_thread() {
        let first = spawn_sh();
        let second = spawn_sh();
        assert_eq!(CHILD_REAPER_STARTS.load(Ordering::SeqCst), 1);
        drop(first);
        drop(second);
    }
    #[test]
    fn inherited_blocked_sigchld_still_reaps_natural_exit() {
        const CHILD_RUN: &str = "LUVUS_BLOCKED_SIGCHLD_REAPER_TEST";
        if std::env::var_os(CHILD_RUN).is_none() {
            // Run this case alone in a fresh process so SIGCHLD is blocked
            // before the process-wide OnceLock and handler are first touched.
            // The child's process-global handler disappears with the child, so
            // parallel tests cannot observe a temporary action.
            let output = std::process::Command::new(std::env::current_exe().expect("test binary"))
                .args([
                    "--exact",
                    "terminal::pty::reap_tests::inherited_blocked_sigchld_still_reaps_natural_exit",
                    "--nocapture",
                ])
                .env(CHILD_RUN, "1")
                .output()
                .expect("run isolated blocked-SIGCHLD test");
            assert!(
                output.status.success(),
                "isolated blocked-SIGCHLD test failed:\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }

        let _mask = SignalMaskGuard::block_sigchld();
        assert_eq!(
            CHILD_REAPER_STARTS.load(Ordering::SeqCst),
            0,
            "the isolated process must block SIGCHLD before reaper setup"
        );

        let (tx, rx) = mpsc::channel();
        let id = PaneId::alloc();
        let pane = Pane::spawn(
            id,
            80,
            24,
            std::env::temp_dir(),
            tx,
            None,
            "/bin/sh",
            500,
            PaneAppearance::default(),
        )
        .expect("spawn shell with inherited blocked SIGCHLD");
        let pid = pane.child_pid.load(Ordering::SeqCst);
        assert_ne!(pid, 0);

        // Let the reaper reach its infinite poll before the child exits. With
        // SIGCHLD inherited as blocked, the old design stranded here forever.
        std::thread::sleep(std::time::Duration::from_millis(200));
        pane.send(b"exit\r");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !pane.child_exited.load(Ordering::SeqCst) {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            assert!(
                !remaining.is_zero(),
                "natural child exit never woke the reaper"
            );
            // The PTY actor may publish PtyExit before the child waiter. Keep
            // waiting until the reaper sets child_exited, which is the behavior
            // this blocked-signal regression is proving.
            match rx.recv_timeout(remaining.min(std::time::Duration::from_millis(50))) {
                Ok(AppEvent::PtyExit(exited)) if exited == id => {}
                Ok(_) | Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(error) => panic!("natural child exit never woke the reaper: {error}"),
            }
        }
        assert!(wait_gone(pid), "naturally exited child was not reaped");
        drop(pane);
    }

    #[test]
    fn dropping_a_live_child_is_still_reaped() {
        let pane = spawn_sh();
        let pid = pane.child_pid.load(Ordering::SeqCst);
        assert_ne!(pid, 0);
        assert!(alive(pid));
        std::thread::sleep(std::time::Duration::from_millis(50));
        drop(pane);
        assert!(wait_gone(pid), "exit is still reaped without a timer");
    }

    /// A deferred pane (docs/82) is fully usable before its shell exists:
    /// input queued right after creation reaches the child once the worker
    /// forks, and the pid stays 0 until then.
    #[test]
    fn deferred_pane_delivers_input_and_spawns_its_child() {
        // Keep the receiver alive: the reader thread exits when its PtyData
        // send fails, which would stop the pump after the first burst.
        let (tx, _rx) = mpsc::channel();
        let pane = Pane::spawn_deferred(
            PaneId::alloc(),
            80,
            24,
            std::env::temp_dir(),
            &[],
            tx,
            "/bin/sh",
            500,
            PaneAppearance::default(),
        );
        assert_eq!(
            pane.child_pid.load(Ordering::SeqCst),
            0,
            "the child is forked off-loop, not at construction"
        );
        // Input sent before the fork must still arrive (queued writer).
        pane.send(b"echo DEFERRED-OK\r");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if pane
                .engine
                .lock()
                .unwrap()
                .detection_text(24)
                .contains("DEFERRED-OK")
            {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the deferred pane never delivered its input"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert_ne!(
            pane.child_pid.load(Ordering::SeqCst),
            0,
            "the worker forked the child"
        );
    }

    #[test]
    fn restored_deferred_pane_replays_saved_screen_immediately() {
        let (tx, _rx) = mpsc::channel();
        let pane = Pane::spawn_restored(
            PaneId::alloc(),
            80,
            24,
            std::env::temp_dir(),
            &[],
            tx,
            Some("RESTORED-SCREEN\r\n"),
            "/bin/sh",
            500,
            PaneAppearance::default(),
        );
        assert!(
            pane.engine
                .lock()
                .unwrap()
                .detection_text(24)
                .contains("RESTORED-SCREEN"),
            "saved content is visible without waiting for the PTY worker"
        );
    }

    #[test]
    fn restored_deferred_pane_retries_a_fallback_cwd() {
        let (tx, rx) = mpsc::channel();
        let id = PaneId::alloc();
        let fallback = std::env::temp_dir();
        let missing = fallback.join(format!("luvus-missing-cwd-{}", std::process::id()));
        let pane = Pane::spawn_restored(
            id,
            80,
            24,
            missing,
            std::slice::from_ref(&fallback),
            tx,
            None,
            "/bin/sh",
            500,
            PaneAppearance::default(),
        );

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            assert!(!remaining.is_zero(), "the fallback PTY never became ready");
            match rx.recv_timeout(remaining) {
                Ok(AppEvent::PtyReady { id: ready, cwd }) if ready == id => {
                    assert_eq!(cwd, fallback);
                    break;
                }
                Ok(AppEvent::PtyExit(exited)) if exited == id => {
                    panic!("the restored pane closed instead of using its fallback cwd")
                }
                Ok(_) => {}
                Err(error) => panic!("the fallback PTY never became ready: {error}"),
            }
        }
        assert_ne!(pane.child_pid.load(Ordering::SeqCst), 0);
    }

    /// A deferred pane (the split path) whose primary cwd was deleted after it
    /// was resolved must retry its fallback chain rather than dying. Regression
    /// for the split that silently disappeared when its cwd vanished in the race
    /// window before the fork: same shape as the restore-path test above, but
    /// driven through `spawn_deferred`.
    #[test]
    fn deferred_pane_retries_a_fallback_cwd() {
        let (tx, rx) = mpsc::channel();
        let id = PaneId::alloc();
        let fallback = std::env::temp_dir();
        let missing = fallback.join(format!("luvus-missing-cwd-{}", std::process::id()));
        let pane = Pane::spawn_deferred(
            id,
            80,
            24,
            missing,
            std::slice::from_ref(&fallback),
            tx,
            "/bin/sh",
            500,
            PaneAppearance::default(),
        );

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            assert!(!remaining.is_zero(), "the fallback PTY never became ready");
            match rx.recv_timeout(remaining) {
                Ok(AppEvent::PtyReady { id: ready, cwd }) if ready == id => {
                    assert_eq!(cwd, fallback);
                    break;
                }
                Ok(AppEvent::PtyExit(exited)) if exited == id => {
                    panic!("the deferred pane closed instead of using its fallback cwd")
                }
                Ok(_) => {}
                Err(error) => panic!("the fallback PTY never became ready: {error}"),
            }
        }
        assert_ne!(pane.child_pid.load(Ordering::SeqCst), 0);
    }

    /// A resize issued before the deferred fork must still reach the child:
    /// the worker opens the PTY at the latest recorded size (docs/82).
    #[test]
    fn deferred_pane_applies_a_resize_racing_the_spawn() {
        let (tx, _rx) = mpsc::channel();
        let mut pane = Pane::spawn_deferred(
            PaneId::alloc(),
            80,
            24,
            std::env::temp_dir(),
            &[],
            tx,
            "/bin/sh",
            500,
            PaneAppearance::default(),
        );
        // The spawn has not forked yet: this is the racing resize.
        assert!(pane.resize(132, 40));
        pane.send(b"stty size\r");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if pane
                .engine
                .lock()
                .unwrap()
                .detection_text(40)
                .contains("40 132")
            {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the child did not see the racing resize; text: {:?}",
                pane.engine.lock().unwrap().detection_text(40)
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    /// Closing a pane must hang up its child. Regression for the leak where the
    /// reader thread's cloned PTY fd kept the child alive, which in turn kept the
    /// engine (and its whole scrollback grid) and both threads alive for the life
    /// of the server: measured 8/8 orphaned shells and ~170 MB never reclaimed.
    #[test]
    fn dropping_a_pane_reaps_its_child() {
        let pane = spawn_sh();
        let pid = pane.child_pid.load(Ordering::SeqCst);
        assert_ne!(pid, 0, "the sync spawn path has a child");
        assert!(alive(pid), "child runs while the pane is open");
        drop(pane);
        assert!(wait_gone(pid), "LEAK: child {pid} survived the pane");
    }

    /// Closing one pane must not disturb its neighbours — the signal is aimed at
    /// one child, not a process group that could take the others with it.
    #[test]
    fn closing_one_pane_leaves_the_others_running() {
        let keep = spawn_sh();
        let kept_pid = keep.child_pid.load(Ordering::SeqCst);
        assert_ne!(kept_pid, 0);
        let doomed = spawn_sh();
        let doomed_pid = doomed.child_pid.load(Ordering::SeqCst);
        assert_ne!(doomed_pid, 0);

        drop(doomed);
        assert!(wait_gone(doomed_pid), "the closed pane's child exited");
        assert!(
            alive(kept_pid),
            "the surviving pane's child is untouched by its neighbour closing"
        );
        drop(keep);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        child_poll_finished, path_with_server_binary, wrap_paste, write_input_action, InputAction,
    };
    use std::path::PathBuf;
    use std::time::Duration;

    #[test]
    fn submit_action_writes_one_paste_then_exactly_one_enter() {
        let mut output = Vec::new();
        write_input_action(
            &mut output,
            InputAction::Submit {
                paste: b"review this".to_vec(),
                settle: Duration::ZERO,
            },
        )
        .unwrap();
        assert_eq!(output, b"review this\r");
    }

    /// A dropped file path must reach the child as a *paste*, not as typing.
    /// luvus receives it with the markers already stripped by crossterm, so it
    /// re-adds them whenever the child enabled DECSET 2004. Without this, an
    /// agent CLI renders the path as literal text instead of attaching the file.
    #[test]
    fn paste_is_bracketed_only_when_the_child_asked() {
        let path = "/Users/riz/shot.png";
        assert_eq!(
            wrap_paste(path, true),
            format!("\x1b[200~{path}\x1b[201~").into_bytes(),
            "wrapped when the child enabled bracketed paste"
        );
        assert_eq!(
            wrap_paste(path, false),
            path.as_bytes(),
            "sent bare when it did not, so a plain shell is unaffected"
        );
    }

    #[test]
    fn paste_preserves_windows_paths_quotes_and_unicode() {
        let command = r#".\.venv\Scripts\python.exe .\youtube_folder_uploader.py --folder "E:\Vídeos\Pendientes €""#;
        assert_eq!(wrap_paste(command, false), command.as_bytes());

        let mut expected = b"\x1b[200~".to_vec();
        expected.extend_from_slice(command.as_bytes());
        expected.extend_from_slice(b"\x1b[201~");
        assert_eq!(wrap_paste(command, true), expected);
    }

    #[test]
    fn reaper_retries_child_poll_errors() {
        assert!(!child_poll_finished(Ok(None)));
        assert!(!child_poll_finished(Err(std::io::Error::other(
            "temporary poll failure"
        ))));
        assert!(child_poll_finished(Ok(Some(
            portable_pty::ExitStatus::with_exit_code(0)
        ))));
    }

    #[test]
    fn pane_path_pins_the_owning_server_binary_portably() {
        let binary_dir = std::env::temp_dir().join("luvus exact binary");
        let exe = binary_dir.join(if cfg!(windows) { "luvus.exe" } else { "luvus" });
        let other = std::env::temp_dir().join("other tools");
        let inherited = std::env::join_paths([other.clone(), binary_dir.clone()]).unwrap();

        let path = path_with_server_binary(&exe, Some(inherited)).expect("portable PATH");
        let entries = std::env::split_paths(&path).collect::<Vec<PathBuf>>();
        assert_eq!(entries.first(), Some(&binary_dir));
        assert_eq!(
            entries.iter().filter(|entry| *entry == &binary_dir).count(),
            1
        );
        assert!(entries.contains(&other), "the user's PATH is preserved");

        let only = path_with_server_binary(&exe, None).expect("PATH without an inherited value");
        assert_eq!(
            std::env::split_paths(&only).collect::<Vec<_>>(),
            [binary_dir]
        );
    }
}
