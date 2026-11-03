//! Runtime configuration, loaded entirely from environment variables so the
//! service is friendly to containers (12-factor style).

use std::env;
use std::time::Duration;

use anyhow::{bail, Context, Result};

/// Fully resolved configuration for a running instance.
#[derive(Clone, Debug)]
pub struct Config {
    /// Address the forward proxy listens on (what the *arr apps point to).
    pub listen: String,
    /// Address the health/status HTTP endpoint listens on.
    pub health_listen: String,
    /// Upstream HTTP proxy to chain to when a block is active (e.g. gluetun).
    pub upstream: String,
    /// How the "is there football / is there a block" decision is made.
    pub mode: Mode,
    /// What to assume when the detector cannot reach its signal source.
    pub fail_mode: FailMode,
    /// Log output format.
    pub log_format: LogFormat,
}

/// Strategy used to decide whether traffic should be routed via the upstream.
#[derive(Clone, Debug)]
pub enum Mode {
    /// Poll hayahora.futbol's `data.json` and apply a threshold + hysteresis.
    HayahoraJson(HayahoraConfig),
    /// Always passthrough (testing / manual override).
    StaticDirect,
    /// Always route via the upstream proxy (testing / manual override).
    StaticBlocked,
}

/// Parameters for the hayahora.futbol polling detector.
#[derive(Clone, Debug)]
pub struct HayahoraConfig {
    pub url: String,
    /// Fraction of monitored IPs that must be "down" to consider a block active.
    pub threshold: f64,
    /// Interval between polls.
    pub interval: Duration,
    /// Consecutive "blocked" readings required to switch TO the upstream (fast).
    pub block_confirm: u32,
    /// Consecutive "clear" readings required to switch BACK to direct (slow).
    pub unblock_confirm: u32,
}

/// Behaviour when the detector's signal source is unreachable.
#[derive(Clone, Copy, Debug)]
pub enum FailMode {
    /// Keep the last known state (default — safest).
    Keep,
    /// Assume there is no block (passthrough).
    Direct,
    /// Assume there is a block (route via upstream).
    Blocked,
}

#[derive(Clone, Copy, Debug)]
pub enum LogFormat {
    Json,
    Pretty,
}

fn var(key: &str, default: &str) -> String {
    env::var(key).unwrap_or_else(|_| default.to_string())
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let listen = var("LALIGA_PROXY_LISTEN", "0.0.0.0:8888");
        let health_listen = var("LALIGA_PROXY_HEALTH_LISTEN", "0.0.0.0:9090");
        let upstream = var(
            "LALIGA_PROXY_UPSTREAM",
            "gluetun.system.svc.cluster.local:8888",
        );

        let mode = match var("LALIGA_PROXY_DETECTOR", "hayahora-json").as_str() {
            "hayahora-json" => {
                let url = var(
                    "LALIGA_PROXY_HAYAHORA_URL",
                    "https://hayahora.futbol/estado/data.json",
                );
                let threshold: f64 = var("LALIGA_PROXY_BLOCK_THRESHOLD", "0.5")
                    .parse()
                    .context("LALIGA_PROXY_BLOCK_THRESHOLD must be a number")?;
                if !(0.0..=1.0).contains(&threshold) {
                    bail!("LALIGA_PROXY_BLOCK_THRESHOLD must be in [0.0, 1.0]");
                }
                let interval_secs: u64 = var("LALIGA_PROXY_POLL_INTERVAL", "30")
                    .parse()
                    .context("LALIGA_PROXY_POLL_INTERVAL must be an integer (seconds)")?;
                let block_confirm: u32 = var("LALIGA_PROXY_BLOCK_CONFIRM", "1")
                    .parse()
                    .context("LALIGA_PROXY_BLOCK_CONFIRM must be an integer")?;
                let unblock_confirm: u32 = var("LALIGA_PROXY_UNBLOCK_CONFIRM", "3")
                    .parse()
                    .context("LALIGA_PROXY_UNBLOCK_CONFIRM must be an integer")?;
                Mode::HayahoraJson(HayahoraConfig {
                    url,
                    threshold,
                    interval: Duration::from_secs(interval_secs.max(1)),
                    block_confirm: block_confirm.max(1),
                    unblock_confirm: unblock_confirm.max(1),
                })
            }
            "static-direct" => Mode::StaticDirect,
            "static-blocked" => Mode::StaticBlocked,
            other => bail!("unknown LALIGA_PROXY_DETECTOR: {other}"),
        };

        let fail_mode = match var("LALIGA_PROXY_FAIL_MODE", "keep").as_str() {
            "keep" => FailMode::Keep,
            "direct" => FailMode::Direct,
            "blocked" => FailMode::Blocked,
            other => bail!("unknown LALIGA_PROXY_FAIL_MODE: {other}"),
        };

        let log_format = match var("LALIGA_PROXY_LOG_FORMAT", "json").as_str() {
            "json" => LogFormat::Json,
            "pretty" | "text" => LogFormat::Pretty,
            other => bail!("unknown LALIGA_PROXY_LOG_FORMAT: {other}"),
        };

        Ok(Config {
            listen,
            health_listen,
            upstream,
            mode,
            fail_mode,
            log_format,
        })
    }

    pub fn mode_name(&self) -> &'static str {
        match self.mode {
            Mode::HayahoraJson(_) => "hayahora-json",
            Mode::StaticDirect => "static-direct",
            Mode::StaticBlocked => "static-blocked",
        }
    }
}
