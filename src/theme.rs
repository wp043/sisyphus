use ratatui::style::Color;
use serde::Deserialize;

/// TUI palette. The default "muted" theme is deliberately low-saturation —
/// desaturated slate/sage/sand instead of pure ANSI primaries.
pub struct Theme {
    pub accent: Color,
    pub ok: Color,
    pub warn: Color,
    pub err: Color,
    pub dim: Color,
    pub text: Color,
    pub highlight_bg: Color,
    pub seq: Color,
    pub fixloop: Color,
    pub failure: Color,
    pub intent: Color,
    pub prompt: Color,
}

impl Theme {
    pub fn muted() -> Self {
        Theme {
            accent: Color::Rgb(122, 140, 178),      // slate blue
            ok: Color::Rgb(140, 168, 138),          // sage
            warn: Color::Rgb(200, 172, 122),        // sand
            err: Color::Rgb(186, 122, 122),         // dusty rose
            dim: Color::Rgb(110, 112, 122),         // cool gray
            text: Color::Rgb(200, 202, 210),
            highlight_bg: Color::Rgb(44, 47, 58),   // charcoal
            seq: Color::Rgb(122, 140, 178),         // slate
            fixloop: Color::Rgb(200, 172, 122),     // sand
            failure: Color::Rgb(186, 122, 122),     // dusty rose
            intent: Color::Rgb(122, 168, 160),      // muted teal
            prompt: Color::Rgb(158, 140, 178),      // dusty lavender
        }
    }

    /// Plain ANSI colors — respects whatever scheme the terminal already has.
    pub fn terminal() -> Self {
        Theme {
            accent: Color::Blue,
            ok: Color::Green,
            warn: Color::Yellow,
            err: Color::Red,
            dim: Color::DarkGray,
            text: Color::Reset,
            highlight_bg: Color::DarkGray,
            seq: Color::Blue,
            fixloop: Color::Yellow,
            failure: Color::Red,
            intent: Color::Cyan,
            prompt: Color::Magenta,
        }
    }

    pub fn kind(&self, kind: &str) -> Color {
        match kind {
            "sequence" => self.seq,
            "fixloop" => self.fixloop,
            "failure" => self.failure,
            "intent" => self.intent,
            "prompt" => self.prompt,
            _ => self.text,
        }
    }
}

#[derive(Deserialize, Default)]
struct ConfigFile {
    #[serde(default)]
    theme: Option<String>,
    #[serde(default)]
    colors: ColorOverrides,
}

#[derive(Deserialize, Default)]
struct ColorOverrides {
    accent: Option<String>,
    ok: Option<String>,
    warn: Option<String>,
    err: Option<String>,
    dim: Option<String>,
    text: Option<String>,
    highlight_bg: Option<String>,
    seq: Option<String>,
    fixloop: Option<String>,
    failure: Option<String>,
    intent: Option<String>,
    prompt: Option<String>,
}

fn parse_hex(s: &str) -> Option<Color> {
    let hex = s.trim().trim_start_matches('#');
    if hex.len() != 6 {
        return None;
    }
    let n = u32::from_str_radix(hex, 16).ok()?;
    Some(Color::Rgb((n >> 16) as u8, (n >> 8) as u8, n as u8))
}

pub fn config_path() -> std::path::PathBuf {
    dirs::home_dir().unwrap_or_default().join(".config/sisyphus/config.toml")
}

/// Load the theme: `theme = "muted" | "terminal"` picks a base, and any key
/// under `[colors]` (hex strings) overrides it.
///
/// ```toml
/// theme = "muted"
/// [colors]
/// accent = "#7a8cb2"
/// ok = "#8ca88a"
/// ```
pub fn load() -> Theme {
    let cfg: ConfigFile = std::fs::read_to_string(config_path())
        .ok()
        .and_then(|t| toml::from_str(&t).ok())
        .unwrap_or_default();
    let mut theme = match cfg.theme.as_deref() {
        Some("terminal") => Theme::terminal(),
        _ => Theme::muted(),
    };
    let o = &cfg.colors;
    for (slot, val) in [
        (&mut theme.accent, &o.accent),
        (&mut theme.ok, &o.ok),
        (&mut theme.warn, &o.warn),
        (&mut theme.err, &o.err),
        (&mut theme.dim, &o.dim),
        (&mut theme.text, &o.text),
        (&mut theme.highlight_bg, &o.highlight_bg),
        (&mut theme.seq, &o.seq),
        (&mut theme.fixloop, &o.fixloop),
        (&mut theme.failure, &o.failure),
        (&mut theme.intent, &o.intent),
        (&mut theme.prompt, &o.prompt),
    ] {
        if let Some(c) = val.as_deref().and_then(parse_hex) {
            *slot = c;
        }
    }
    theme
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hex() {
        assert_eq!(parse_hex("#7a8cb2"), Some(Color::Rgb(0x7a, 0x8c, 0xb2)));
        assert_eq!(parse_hex("7a8cb2"), Some(Color::Rgb(0x7a, 0x8c, 0xb2)));
        assert_eq!(parse_hex("#zzz"), None);
    }

    #[test]
    fn overrides_apply() {
        let cfg: ConfigFile =
            toml::from_str("theme = \"terminal\"\n[colors]\naccent = \"#112233\"").unwrap();
        assert_eq!(cfg.theme.as_deref(), Some("terminal"));
        assert_eq!(parse_hex(cfg.colors.accent.as_deref().unwrap()), Some(Color::Rgb(0x11, 0x22, 0x33)));
    }
}
