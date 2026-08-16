use chrono::Timelike;
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
            // A cool, pale ice-blue rather than neutral grey or stark
            // white: it reads like a backlit LCD panel rather than
            // printed ink.
            digit_color: (0xDC, 0xE8, 0xF5),
            background_color: (0x00, 0x00, 0x00),
            card_color: (0x0F, 0x0F, 0x0F),
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
    /// Formats the current time as exactly 5 characters (H H : M M), which
    /// is what the fixed 4-digit-slot layout requires. Computed straight
    /// from `Timelike` accessors rather than chrono's string formatter:
    /// this runs on every animation frame while a flip is in progress (up
    /// to 60 times a second), and formatting-then-parsing a string on
    /// that path would allocate for no reason.
    ///
    /// In 12-hour mode the hour is space-padded rather than zero-padded (a
    /// lone "7", not "07"): real flip clocks with a two-flap hour card
    /// leave the tens flap blank rather than printing a leading zero, and
    /// a space character rasterizes to an empty glyph, so the blank flap
    /// falls out of the normal digit-rendering path for free.
    pub fn format_now(self) -> [char; 5] {
        let now = chrono::Local::now();
        let (hour, zero_pad_hour) = match self {
            HourFormat::TwentyFour => (now.hour(), true),
            HourFormat::Twelve => (now.hour12().1, false),
        };
        let minute = now.minute();
        let h_tens = if zero_pad_hour || hour >= 10 { char::from_digit(hour / 10, 10).unwrap() } else { ' ' };
        let h_ones = char::from_digit(hour % 10, 10).unwrap();
        let m_tens = char::from_digit(minute / 10, 10).unwrap();
        let m_ones = char::from_digit(minute % 10, 10).unwrap();
        [h_tens, h_ones, ':', m_tens, m_ones]
    }

    /// Whether an AM/PM indicator should be shown alongside the clock.
    /// Meaningless (and skipped) in 24-hour mode, where the hour alone
    /// already disambiguates.
    pub fn shows_meridiem(self) -> bool {
        matches!(self, HourFormat::Twelve)
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
