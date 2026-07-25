use serde::Deserialize;

/// User config from `~/.config/sisyphus/config.toml`. Every field is optional
/// with a sensible default, so an absent or partial file is fine.
#[derive(Deserialize, Default)]
pub struct Config {
    #[serde(default)]
    pub mine: Mine,
    #[serde(default)]
    pub report: Report,
}

#[derive(Deserialize, Default)]
pub struct Mine {
    /// Hide command patterns whose estimated hand-time is below this (minutes).
    #[serde(default)]
    pub min_minutes: f64,
}

#[derive(Deserialize, Default)]
pub struct Report {
    /// Default patterns shown per kind (the `--limit` flag overrides).
    pub limit: Option<usize>,
}

pub fn load() -> Config {
    std::fs::read_to_string(crate::theme::config_path())
        .ok()
        .and_then(|t| toml::from_str(&t).ok())
        .unwrap_or_default()
}
