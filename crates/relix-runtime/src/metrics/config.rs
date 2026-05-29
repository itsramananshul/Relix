//! `[metrics]` controller config for RELIX-7.11.
//!
//! ```toml
//! [metrics]
//! enabled = true
//! db_path = "/var/lib/relix/metrics.sqlite"  # default: <data_dir>/metrics.sqlite
//! retention_days = 30
//! retention_sweep_interval_secs = 3600        # 1h default
//!
//! [metrics.thresholds]
//! error_rate_pct = 10.0
//! p95_latency_ms = 5000
//! cost_per_hour_micros = 1_000_000            # $1.00
//! zero_success_window_mins = 10
//!
//! [metrics.prices]
//! "gpt-4o-mini" = { prompt_per_1k_micros = 150, completion_per_1k_micros = 600 }
//! "claude-sonnet-4" = { prompt_per_1k_micros = 3000, completion_per_1k_micros = 15000 }
//! ```

use std::path::PathBuf;

use serde::Deserialize;

use super::alert::AlertThresholds;
use super::alert_delivery::AlertDeliveryConfig;
use super::pricing::PriceTableConfig;

/// Default location for the metrics SQLite store beneath a data
/// dir. Mirrors `default_chronicle_path` in the workflow module.
pub fn default_metrics_path(data_dir: &std::path::Path) -> PathBuf {
    super::store::default_metrics_path(data_dir)
}

#[derive(Clone, Debug, Deserialize)]
pub struct MetricsConfig {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// Path to the metrics SQLite file. When unset the
    /// controller drops it next to the audit log.
    #[serde(default)]
    pub db_path: Option<PathBuf>,
    /// Days of metric retention. Default 30.
    #[serde(default = "default_retention_days")]
    pub retention_days: u32,
    /// How often the retention loop sweeps. Default 1h.
    #[serde(default = "default_retention_sweep_interval_secs")]
    pub retention_sweep_interval_secs: u64,
    /// Alert threshold knobs. Defaults match the spec.
    #[serde(default)]
    pub thresholds: AlertThresholds,
    /// Per-model price overrides; merged on top of the
    /// built-in defaults.
    #[serde(default)]
    pub prices: PriceTableConfig,
    /// Interval between alert evaluations. Default 60s.
    #[serde(default = "default_alert_interval_secs")]
    pub alert_interval_secs: u64,
    /// `[metrics.alerts]` — fan-out + chronicle. Absent means
    /// the chronicle still writes to the default
    /// `<data_dir>/alerts.sqlite` path; only the channel
    /// fan-out stays dormant.
    #[serde(default)]
    pub alerts: AlertDeliveryConfig,
    /// `[metrics.cost_alerts]` — GAP 22 Feature 2 persistent
    /// baseline + spike-detector knobs. Absent / `enabled =
    /// false` keeps the detector dormant; the existing
    /// AlertEngine evaluators in `super::alert` continue to
    /// run on their own poll cycle.
    #[serde(default)]
    pub cost_alerts: super::spike_detector::CostAlertsConfig,
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self {
            enabled: default_enabled(),
            db_path: None,
            retention_days: default_retention_days(),
            retention_sweep_interval_secs: default_retention_sweep_interval_secs(),
            thresholds: AlertThresholds::default(),
            prices: PriceTableConfig::default(),
            alert_interval_secs: default_alert_interval_secs(),
            alerts: AlertDeliveryConfig::default(),
            cost_alerts: super::spike_detector::CostAlertsConfig::default(),
        }
    }
}

fn default_enabled() -> bool {
    true
}
fn default_retention_days() -> u32 {
    30
}
fn default_retention_sweep_interval_secs() -> u64 {
    3600
}
fn default_alert_interval_secs() -> u64 {
    60
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_section_with_defaults() {
        let cfg: MetricsConfig = toml::from_str("enabled = true").unwrap();
        assert!(cfg.enabled);
        assert_eq!(cfg.retention_days, 30);
        assert_eq!(cfg.retention_sweep_interval_secs, 3600);
        assert_eq!(cfg.alert_interval_secs, 60);
        // Thresholds default to the spec.
        assert!(cfg.thresholds.error_rate_pct > 0.0);
        assert!(cfg.prices.entries.is_empty());
    }

    #[test]
    fn parses_full_section_with_overrides() {
        let toml_text = r#"
            enabled = true
            db_path = "/tmp/metrics.sqlite"
            retention_days = 7
            retention_sweep_interval_secs = 60
            alert_interval_secs = 30

            [thresholds]
            error_rate_pct = 5.0
            p95_latency_ms = 2000
            cost_per_hour_micros = 500000
            zero_success_window_mins = 5

            [prices]
            "gpt-4o-mini" = { prompt_per_1k_micros = 100, completion_per_1k_micros = 200 }
        "#;
        let cfg: MetricsConfig = toml::from_str(toml_text).unwrap();
        assert_eq!(cfg.retention_days, 7);
        assert_eq!(cfg.thresholds.error_rate_pct, 5.0);
        assert_eq!(cfg.thresholds.p95_latency_ms, 2000);
        assert_eq!(cfg.alert_interval_secs, 30);
        let prices = cfg.prices.clone().into_table();
        let p = prices.get("gpt-4o-mini").unwrap();
        assert_eq!(p.prompt_per_1k_micros, 100);
    }

    #[test]
    fn default_is_enabled_with_30_day_retention() {
        let cfg = MetricsConfig::default();
        assert!(cfg.enabled);
        assert_eq!(cfg.retention_days, 30);
    }
}
