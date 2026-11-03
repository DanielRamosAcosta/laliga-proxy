//! Shared runtime state: the current routing decision plus lightweight
//! counters, all cheap to read from every connection handler.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;

use serde::Serialize;

/// Snapshot of the last detector poll, exposed on the status endpoint.
#[derive(Default, Clone, Serialize)]
pub struct PollInfo {
    pub last_poll_ok: bool,
    pub last_blocked_ratio: f64,
    pub last_update: Option<String>,
    pub last_error: Option<String>,
}

pub struct SharedState {
    blocked: AtomicBool,
    total_conns: AtomicU64,
    direct_conns: AtomicU64,
    upstream_conns: AtomicU64,
    poll: Mutex<PollInfo>,
}

impl SharedState {
    pub fn new() -> Self {
        Self {
            blocked: AtomicBool::new(false),
            total_conns: AtomicU64::new(0),
            direct_conns: AtomicU64::new(0),
            upstream_conns: AtomicU64::new(0),
            poll: Mutex::new(PollInfo::default()),
        }
    }

    pub fn is_blocked(&self) -> bool {
        self.blocked.load(Ordering::Relaxed)
    }

    /// Set the routing state; returns `true` if the value actually changed.
    pub fn set_blocked(&self, value: bool) -> bool {
        self.blocked.swap(value, Ordering::Relaxed) != value
    }

    pub fn inc_total(&self) {
        self.total_conns.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_direct(&self) {
        self.direct_conns.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_upstream(&self) {
        self.upstream_conns.fetch_add(1, Ordering::Relaxed);
    }

    pub fn last_blocked_ratio(&self) -> f64 {
        self.poll.lock().unwrap().last_blocked_ratio
    }

    pub fn update_poll(&self, info: PollInfo) {
        *self.poll.lock().unwrap() = info;
    }

    pub fn snapshot(&self) -> StatusSnapshot {
        let poll = self.poll.lock().unwrap().clone();
        let blocked = self.is_blocked();
        StatusSnapshot {
            blocked,
            route: if blocked { "upstream" } else { "direct" },
            total_connections: self.total_conns.load(Ordering::Relaxed),
            direct_connections: self.direct_conns.load(Ordering::Relaxed),
            upstream_connections: self.upstream_conns.load(Ordering::Relaxed),
            poll,
        }
    }
}

impl Default for SharedState {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Serialize)]
pub struct StatusSnapshot {
    pub blocked: bool,
    pub route: &'static str,
    pub total_connections: u64,
    pub direct_connections: u64,
    pub upstream_connections: u64,
    pub poll: PollInfo,
}
