//! RELIX-7.15 — `[training]` controller config.
//!
//! ```toml
//! [training]
//! enabled = true
//! db_path = "/var/lib/relix/training.sqlite"   # default: <data_dir>/training.sqlite
//! retention_days = 90
//! retention_sweep_interval_secs = 86_400        # 1d default
//!
//! scorer_enabled = true
//! scorer_interval_secs = 30
//! scorer_batch_size = 50
//!
//! export_dir = "/var/lib/relix/training_exports"
//! min_quality_score = 0.7
//! ```

use std::path::PathBuf;

use serde::Deserialize;

use super::pii::PiiConfig;

/// Default location for the training database next to a
/// controller's data dir. Mirrors `default_metrics_path`.
pub fn default_training_path(data_dir: &std::path::Path) -> PathBuf {
    super::store::default_training_path(data_dir)
}

/// Default location for the export output directory next to a
/// controller's data dir.
pub fn default_export_dir(data_dir: &std::path::Path) -> PathBuf {
    super::exporter::default_export_dir(data_dir)
}

#[derive(Clone, Debug, Deserialize)]
pub struct TrainingConfig {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default)]
    pub db_path: Option<PathBuf>,
    #[serde(default = "default_retention_days")]
    pub retention_days: u32,
    #[serde(default = "default_retention_sweep_interval_secs")]
    pub retention_sweep_interval_secs: u64,
    #[serde(default = "default_scorer_enabled")]
    pub scorer_enabled: bool,
    #[serde(default = "default_scorer_interval_secs")]
    pub scorer_interval_secs: u64,
    #[serde(default = "default_scorer_batch_size")]
    pub scorer_batch_size: u32,
    #[serde(default)]
    pub export_dir: Option<PathBuf>,
    #[serde(default = "default_min_quality_score")]
    pub min_quality_score: f32,
    /// `[training.pii]` — RELIX-7.15 PII anonymization layer.
    /// Absent / disabled means recorder + exporter run without
    /// the redaction pass, matching the pre-PII pipeline shape.
    #[serde(default)]
    pub pii: PiiConfig,
}

impl Default for TrainingConfig {
    fn default() -> Self {
        Self {
            enabled: default_enabled(),
            db_path: None,
            retention_days: default_retention_days(),
            retention_sweep_interval_secs: default_retention_sweep_interval_secs(),
            scorer_enabled: default_scorer_enabled(),
            scorer_interval_secs: default_scorer_interval_secs(),
            scorer_batch_size: default_scorer_batch_size(),
            export_dir: None,
            min_quality_score: default_min_quality_score(),
            pii: PiiConfig::default(),
        }
    }
}

fn default_enabled() -> bool {
    true
}
fn default_retention_days() -> u32 {
    90
}
fn default_retention_sweep_interval_secs() -> u64 {
    86_400
}
fn default_scorer_enabled() -> bool {
    true
}
fn default_scorer_interval_secs() -> u64 {
    30
}
fn default_scorer_batch_size() -> u32 {
    50
}
fn default_min_quality_score() -> f32 {
    0.7
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_section_with_defaults() {
        let cfg: TrainingConfig = toml::from_str("enabled = true").unwrap();
        assert!(cfg.enabled);
        assert_eq!(cfg.retention_days, 90);
        assert_eq!(cfg.scorer_batch_size, 50);
        assert!((cfg.min_quality_score - 0.7).abs() < 1e-4);
    }

    #[test]
    fn parses_full_section_with_overrides() {
        let toml_text = r#"
            enabled = true
            db_path = "/tmp/training.sqlite"
            retention_days = 30
            retention_sweep_interval_secs = 3600
            scorer_enabled = false
            scorer_interval_secs = 90
            scorer_batch_size = 100
            export_dir = "/tmp/exports"
            min_quality_score = 0.5
        "#;
        let cfg: TrainingConfig = toml::from_str(toml_text).unwrap();
        assert_eq!(cfg.retention_days, 30);
        assert!(!cfg.scorer_enabled);
        assert_eq!(cfg.scorer_batch_size, 100);
        assert!((cfg.min_quality_score - 0.5).abs() < 1e-4);
    }

    #[test]
    fn defaults_have_recorder_enabled_with_90_day_retention() {
        let cfg = TrainingConfig::default();
        assert!(cfg.enabled);
        assert_eq!(cfg.retention_days, 90);
        assert_eq!(cfg.scorer_interval_secs, 30);
    }
}
