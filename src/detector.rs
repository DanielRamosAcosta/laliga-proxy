//! The detector decides whether LaLiga is currently blocking traffic.
//!
//! hayahora.futbol does not know whether football is on — it monitors a set of
//! Cloudflare IPs per ISP and records when they become unreachable. A block
//! (i.e. "there is football") manifests as a large fraction of those IPs going
//! `down`. We poll that signal, apply a threshold, and debounce the result with
//! asymmetric hysteresis so we switch to the VPN quickly but switch back
//! conservatively.

use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;

use crate::config::{Config, FailMode, HayahoraConfig, Mode};
use crate::state::{PollInfo, SharedState};

#[derive(Debug, Deserialize)]
pub struct HayahoraData {
    #[serde(rename = "lastUpdate")]
    pub last_update: Option<String>,
    #[serde(default)]
    pub data: Vec<IpEntry>,
}

#[derive(Debug, Deserialize)]
pub struct IpEntry {
    #[serde(rename = "stateChanges", default)]
    pub state_changes: Vec<StateChange>,
}

#[derive(Debug, Deserialize)]
pub struct StateChange {
    /// `true` = IP reachable, `false` = blocked/unreachable.
    pub state: bool,
}

/// Fraction of monitored IPs whose latest known state is "down" (blocked).
/// IPs with no recorded state are ignored.
pub fn blocked_ratio(data: &HayahoraData) -> f64 {
    let mut total = 0u64;
    let mut blocked = 0u64;
    for entry in &data.data {
        if let Some(last) = entry.state_changes.last() {
            total += 1;
            if !last.state {
                blocked += 1;
            }
        }
    }
    if total == 0 {
        0.0
    } else {
        blocked as f64 / total as f64
    }
}

/// Asymmetric debounce: requires `block_confirm` consecutive positive readings
/// to flip on, and `unblock_confirm` consecutive negative readings to flip off.
pub struct Hysteresis {
    block_confirm: u32,
    unblock_confirm: u32,
    blocked: bool,
    run_blocked: u32,
    run_unblocked: u32,
}

impl Hysteresis {
    pub fn new(block_confirm: u32, unblock_confirm: u32) -> Self {
        Self {
            block_confirm: block_confirm.max(1),
            unblock_confirm: unblock_confirm.max(1),
            blocked: false,
            run_blocked: 0,
            run_unblocked: 0,
        }
    }

    /// Feed one instantaneous reading. Returns `Some(new_state)` only on an
    /// actual debounced transition, otherwise `None`.
    pub fn observe(&mut self, reading_blocked: bool) -> Option<bool> {
        if reading_blocked {
            self.run_blocked = self.run_blocked.saturating_add(1);
            self.run_unblocked = 0;
        } else {
            self.run_unblocked = self.run_unblocked.saturating_add(1);
            self.run_blocked = 0;
        }

        let transition =
            if !self.blocked && reading_blocked && self.run_blocked >= self.block_confirm {
                Some(true)
            } else if self.blocked && !reading_blocked && self.run_unblocked >= self.unblock_confirm
            {
                Some(false)
            } else {
                None
            };

        if let Some(value) = transition {
            self.blocked = value;
        }
        transition
    }

    #[cfg(test)]
    pub fn state(&self) -> bool {
        self.blocked
    }
}

pub async fn run(cfg: Arc<Config>, state: Arc<SharedState>) {
    match &cfg.mode {
        Mode::StaticDirect => {
            state.set_blocked(false);
            tracing::info!(
                event = "detector_static",
                mode = "static-direct",
                "static mode: always passthrough"
            );
        }
        Mode::StaticBlocked => {
            state.set_blocked(true);
            tracing::info!(
                event = "detector_static",
                mode = "static-blocked",
                "static mode: always via upstream"
            );
        }
        Mode::HayahoraJson(hc) => run_hayahora(hc.clone(), cfg.fail_mode, state).await,
    }
}

async fn run_hayahora(hc: HayahoraConfig, fail_mode: FailMode, state: Arc<SharedState>) {
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .user_agent(concat!("laliga-proxy/", env!("CARGO_PKG_VERSION")))
        .build()
    {
        Ok(client) => client,
        Err(e) => {
            tracing::error!(event = "detector_init_error", error = %e, "failed to build HTTP client");
            return;
        }
    };

    let mut hyst = Hysteresis::new(hc.block_confirm, hc.unblock_confirm);
    let mut ticker = tokio::time::interval(hc.interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    tracing::info!(
        event = "detector_start",
        url = %hc.url,
        threshold = hc.threshold,
        interval_secs = hc.interval.as_secs(),
        block_confirm = hc.block_confirm,
        unblock_confirm = hc.unblock_confirm,
        "hayahora detector running"
    );

    loop {
        ticker.tick().await;
        match poll_once(&client, &hc.url).await {
            Ok(data) => {
                let ratio = blocked_ratio(&data);
                let reading = ratio >= hc.threshold;
                tracing::debug!(
                    event = "poll_ok",
                    blocked_ratio = ratio,
                    reading_blocked = reading,
                    "hayahora poll"
                );
                state.update_poll(PollInfo {
                    last_poll_ok: true,
                    last_blocked_ratio: ratio,
                    last_update: data.last_update.clone(),
                    last_error: None,
                });
                if let Some(new_state) = hyst.observe(reading) {
                    state.set_blocked(new_state);
                    if new_state {
                        tracing::info!(
                            event = "state_change",
                            blocked = true,
                            route = "upstream",
                            blocked_ratio = ratio,
                            "¡hay futbol! routing via upstream (gluetun)"
                        );
                    } else {
                        tracing::info!(
                            event = "state_change",
                            blocked = false,
                            route = "direct",
                            blocked_ratio = ratio,
                            "se acabo el futbol — passthrough directo"
                        );
                    }
                }
            }
            Err(e) => {
                tracing::warn!(
                    event = "poll_error",
                    error = %e,
                    fail_mode = ?fail_mode,
                    "hayahora poll failed"
                );
                state.update_poll(PollInfo {
                    last_poll_ok: false,
                    last_blocked_ratio: state.last_blocked_ratio(),
                    last_update: None,
                    last_error: Some(e.to_string()),
                });
                match fail_mode {
                    FailMode::Keep => {}
                    FailMode::Direct => {
                        if state.set_blocked(false) {
                            tracing::info!(
                                event = "state_change",
                                blocked = false,
                                reason = "fail_mode",
                                "poll failing: forcing direct"
                            );
                        }
                    }
                    FailMode::Blocked => {
                        if state.set_blocked(true) {
                            tracing::info!(
                                event = "state_change",
                                blocked = true,
                                reason = "fail_mode",
                                "poll failing: forcing upstream"
                            );
                        }
                    }
                }
            }
        }
    }
}

async fn poll_once(client: &reqwest::Client, url: &str) -> anyhow::Result<HayahoraData> {
    let resp = client.get(url).send().await?.error_for_status()?;
    Ok(resp.json::<HayahoraData>().await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data(states: &[&[bool]]) -> HayahoraData {
        HayahoraData {
            last_update: None,
            data: states
                .iter()
                .map(|changes| IpEntry {
                    state_changes: changes.iter().map(|s| StateChange { state: *s }).collect(),
                })
                .collect(),
        }
    }

    #[test]
    fn ratio_uses_latest_state_per_ip() {
        // ip1 ends down, ip2 ends up, ip3 ends down
        let d = data(&[&[true, false], &[false, true], &[true, true, false]]);
        assert!((blocked_ratio(&d) - (2.0 / 3.0)).abs() < 1e-9);
    }

    #[test]
    fn ratio_ignores_ips_without_changes() {
        let d = data(&[&[], &[false], &[]]);
        assert!((blocked_ratio(&d) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn ratio_empty_is_zero() {
        let d = data(&[]);
        assert_eq!(blocked_ratio(&d), 0.0);
    }

    #[test]
    fn parses_real_shape() {
        let json = r#"{
            "lastUpdate": "2026-11-03 19:28:05Z",
            "data": [
                {"ip":"104.16.93.114","isp":"DIGI","description":"Cloudflare",
                 "stateChanges":[{"timestamp":"t","state":true},{"timestamp":"t2","state":false}]}
            ]
        }"#;
        let parsed: HayahoraData = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.last_update.as_deref(), Some("2026-11-03 19:28:05Z"));
        assert_eq!(blocked_ratio(&parsed), 1.0);
    }

    #[test]
    fn hysteresis_fast_on_slow_off() {
        // block_confirm=1 (switch on immediately), unblock_confirm=3
        let mut h = Hysteresis::new(1, 3);
        assert_eq!(h.observe(true), Some(true));
        assert!(h.state());
        // one clear reading is not enough to switch back
        assert_eq!(h.observe(false), None);
        assert_eq!(h.observe(false), None);
        assert_eq!(h.observe(false), Some(false));
        assert!(!h.state());
    }

    #[test]
    fn hysteresis_requires_consecutive_readings() {
        let mut h = Hysteresis::new(2, 2);
        assert_eq!(h.observe(true), None); // 1 blocked
        assert_eq!(h.observe(false), None); // resets blocked run
        assert_eq!(h.observe(true), None); // 1 blocked again
        assert_eq!(h.observe(true), Some(true)); // 2 consecutive -> on
    }

    #[test]
    fn hysteresis_no_spurious_transitions() {
        let mut h = Hysteresis::new(1, 1);
        assert_eq!(h.observe(false), None); // already off
        assert_eq!(h.observe(true), Some(true));
        assert_eq!(h.observe(true), None); // already on
    }
}
