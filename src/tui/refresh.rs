//! Background index-refresh thread for the TUI.
//!
//! Owns no borrowed `Storage` -- creates a fresh `Indexer` per refresh cycle
//! to keep writes isolated from the TUI's read-side storage handle. Uses
//! `std::thread` + `std::sync::mpsc`, matching the existing search thread.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::thread;
use std::time::{Duration, Instant};

use crate::indexer::{IndexStats, Indexer};

/// Static configuration for the TUI's refresh thread.
#[derive(Debug, Clone)]
pub struct RefreshConfig {
    pub data_dir: PathBuf,
    pub enable_semantic: bool,
    pub max_age: Duration,
    pub interval: Duration,
    /// If false, the refresh thread is never started.
    pub enabled: bool,
}

/// Events the refresh thread sends to the TUI.
#[derive(Debug)]
pub enum RefreshEvent {
    Started,
    Finished {
        stats: IndexStats,
        deleted: usize,
        uncertain: usize,
    },
    SkippedFresh,
    BusySkipped,
    Failed(String),
}

/// Commands sent from the TUI to the refresh thread.
#[derive(Debug)]
enum Command {
    Tick,
    Stop,
}

pub struct RefreshThread {
    cmd_tx: Sender<Command>,
    event_rx: Receiver<RefreshEvent>,
    busy: Arc<AtomicBool>,
    enabled: bool,
}

impl RefreshThread {
    /// Spawn a refresh thread. If `cfg.enabled` is false, returns a disabled
    /// handle whose `try_recv` / `tick` are no-ops.
    pub fn spawn(cfg: RefreshConfig) -> Self {
        let (cmd_tx, cmd_rx) = channel::<Command>();
        let (event_tx, event_rx) = channel::<RefreshEvent>();
        let busy = Arc::new(AtomicBool::new(false));

        if !cfg.enabled {
            return Self {
                cmd_tx,
                event_rx,
                busy,
                enabled: false,
            };
        }

        let busy_clone = busy.clone();
        thread::spawn(move || {
            // Initial refresh on launch if stale.
            run_refresh_if_stale(&cfg, &event_tx, &busy_clone);

            let mut last_tick = Instant::now();
            loop {
                // Use recv_timeout so we tick on cfg.interval but also react
                // to explicit Tick/Stop commands.
                match cmd_rx.recv_timeout(cfg.interval) {
                    Ok(Command::Stop) => break,
                    Ok(Command::Tick) => {
                        run_refresh_if_stale(&cfg, &event_tx, &busy_clone);
                        last_tick = Instant::now();
                    }
                    Err(_) => {
                        // Timeout -> regular tick
                        if last_tick.elapsed() >= cfg.interval {
                            run_refresh_if_stale(&cfg, &event_tx, &busy_clone);
                            last_tick = Instant::now();
                        }
                    }
                }
            }
        });

        Self {
            cmd_tx,
            event_rx,
            busy,
            enabled: true,
        }
    }

    pub fn try_recv(&self) -> Option<RefreshEvent> {
        match self.event_rx.try_recv() {
            Ok(ev) => Some(ev),
            Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => None,
        }
    }

    pub fn is_busy(&self) -> bool {
        self.busy.load(Ordering::SeqCst)
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Request an immediate refresh attempt (no-op if disabled or busy).
    pub fn request_tick(&self) {
        let _ = self.cmd_tx.send(Command::Tick);
    }
}

impl Drop for RefreshThread {
    fn drop(&mut self) {
        let _ = self.cmd_tx.send(Command::Stop);
    }
}

fn run_refresh_if_stale(cfg: &RefreshConfig, tx: &Sender<RefreshEvent>, busy: &Arc<AtomicBool>) {
    // Don't overlap.
    if busy.swap(true, Ordering::SeqCst) {
        let _ = tx.send(RefreshEvent::BusySkipped);
        return;
    }

    let result = (|| -> anyhow::Result<RefreshEvent> {
        let mut indexer = Indexer::new(&cfg.data_dir, cfg.enable_semantic)?;
        if indexer.needs_initial_index()? {
            let _ = tx.send(RefreshEvent::Started);
            let stats = indexer.full_index()?;
            return Ok(RefreshEvent::Finished {
                stats,
                deleted: 0,
                uncertain: 0,
            });
        }
        if !indexer.should_refresh(cfg.max_age)? {
            return Ok(RefreshEvent::SkippedFresh);
        }
        let _ = tx.send(RefreshEvent::Started);
        let stats = refresh_with_retry(&mut indexer, 3)?;
        Ok(RefreshEvent::Finished {
            stats,
            deleted: 0,
            uncertain: 0,
        })
    })();

    busy.store(false, Ordering::SeqCst);

    let ev = match result {
        Ok(ev) => ev,
        Err(e) => {
            let msg = format!("{:#}", e);
            if msg.contains("database is locked") || msg.contains("LockBusy") {
                RefreshEvent::BusySkipped
            } else {
                RefreshEvent::Failed(msg)
            }
        }
    };
    let _ = tx.send(ev);
}

fn refresh_with_retry(indexer: &mut Indexer, attempts: u32) -> anyhow::Result<IndexStats> {
    let mut last_err = None;
    for attempt in 0..attempts {
        match indexer.incremental_index() {
            Ok(stats) => return Ok(stats),
            Err(e) => {
                let msg = format!("{:#}", e);
                let busy = msg.contains("database is locked") || msg.contains("LockBusy");
                if busy && attempt + 1 < attempts {
                    thread::sleep(Duration::from_millis(200 * (attempt as u64 + 1)));
                    last_err = Some(e);
                    continue;
                }
                return Err(e);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("refresh: unknown failure")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_thread_emits_nothing() {
        let t = RefreshThread::spawn(RefreshConfig {
            data_dir: PathBuf::from("/tmp/sess-noop"),
            enable_semantic: false,
            max_age: Duration::from_secs(60),
            interval: Duration::from_secs(60),
            enabled: false,
        });
        assert!(!t.enabled());
        assert!(t.try_recv().is_none());
    }

    #[test]
    fn refresh_event_finished_carries_stats() {
        // Smoke test: variant constructs and is sendable across mpsc.
        let (tx, rx) = channel();
        tx.send(RefreshEvent::Finished {
            stats: IndexStats::default(),
            deleted: 2,
            uncertain: 1,
        })
        .unwrap();
        match rx.recv().unwrap() {
            RefreshEvent::Finished {
                deleted, uncertain, ..
            } => {
                assert_eq!(deleted, 2);
                assert_eq!(uncertain, 1);
            }
            _ => panic!("wrong variant"),
        }
    }
}
