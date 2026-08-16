use serde::Deserialize;

/// User-facing settings, loaded from ~/.config/wayqlo/config.toml.
/// `#[serde(default)]` on the struct means: if a field is missing from the
/// file (or the whole file is missing), fall back to `Config::default()`
/// for that field — so a user's config only needs to mention what they
/// actually want to change.
#[derive(Deserialize)]
#[serde(default)]
pub struct Config {
    pub hour_format: HourFormat,
    #[serde(deserialize_with = "deserialize_color")]
    pub digit_color: (u8, u8, u8),
    #[serde(deserialize_with = "deserialize_color")]
    pub background_color: (u8, u8, u8),
    /// The flip-card panel color, distinct from the surrounding
    /// background — this is what makes it read as physical cards sitting
    /// on the desktop rather than digits floating in empty space.
    #[serde(deserialize_with = "deserialize_color")]
    pub card_color: (u8, u8, u8),
}

impl Default for Config {
    fn default() -> Self {
        Config {
            hour_format: HourFormat::TwentyFour,
            digit_color: (0xFF, 0xFF, 0xFF),
            background_color: (0x00, 0x00, 0x00),
            card_color: (0x16, 0x16, 0x16),
        }
    }
}

#[derive(Deserialize, Clone, Copy)]
pub enum HourFormat {
    #[serde(rename = "12")]
    Twelve,
    #[serde(rename = "24")]
    TwentyFour,
}

impl HourFormat {
    /// The chrono format string for this hour style. Always 2-digit hour +
    /// 2-digit minute — our layout has 5 fixed character slots (H H : M M)
    /// and can't yet support a variable-width string.
    pub fn strftime(self) -> &'static str {
        match self {
            HourFormat::Twelve => "%I:%M",
            HourFormat::TwentyFour => "%H:%M",
        }
    }
}

fn deserialize_color<'de, D>(deserializer: D) -> Result<(u8, u8, u8), D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s = String::deserialize(deserializer)?;
    parse_hex_color(&s)
        .ok_or_else(|| serde::de::Error::custom(format!("invalid color {s:?}, expected \"#RRGGBB\"")))
}

fn parse_hex_color(s: &str) -> Option<(u8, u8, u8)> {
    let s = s.strip_prefix('#')?;
    if s.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&s[0..2], 16).ok()?;
    let g = u8::from_str_radix(&s[2..4], 16).ok()?;
    let b = u8::from_str_radix(&s[4..6], 16).ok()?;
    Some((r, g, b))
}

/// Loads config from ~/.config/wayqlo/config.toml. A missing file just
/// means defaults — that's the expected state for most users, not an
/// error. A malformed file prints a warning and also falls back to
/// defaults, rather than crashing a screensaver over a typo.
pub fn load() -> Config {
    let Ok(home) = std::env::var("HOME") else {
        return Config::default();
    };
    let path = std::path::Path::new(&home).join(".config/wayqlo/config.toml");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Config::default();
    };
    match toml::from_str(&text) {
        Ok(config) => config,
        Err(e) => {
            eprintln!("wayqlo: failed to parse {}: {e}, using defaults", path.display());
            Config::default()
        }
    }
}
