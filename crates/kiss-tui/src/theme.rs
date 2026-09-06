//! Themes: JSON files mapping color tokens to hex/256-color values, with a
//! `vars` indirection layer. Built-in dark and light themes.

use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Color {
    Default,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

impl Color {
    pub fn fg_code(&self) -> String {
        match self {
            Color::Default => "\x1b[39m".into(),
            Color::Indexed(i) => format!("\x1b[38;5;{i}m"),
            Color::Rgb(r, g, b) => format!("\x1b[38;2;{r};{g};{b}m"),
        }
    }

    pub fn bg_code(&self) -> String {
        match self {
            Color::Default => "\x1b[49m".into(),
            Color::Indexed(i) => format!("\x1b[48;5;{i}m"),
            Color::Rgb(r, g, b) => format!("\x1b[48;2;{r};{g};{b}m"),
        }
    }

    fn parse(value: &str) -> Option<Color> {
        if value.is_empty() {
            return Some(Color::Default);
        }
        if let Some(hex) = value.strip_prefix('#')
            && hex.len() == 6
        {
            let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
            let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
            return Some(Color::Rgb(r, g, b));
        }
        value.parse::<u8>().ok().map(Color::Indexed)
    }
}

#[derive(Debug, Clone)]
pub struct Theme {
    pub name: String,
    colors: BTreeMap<String, Color>,
}

#[derive(Deserialize)]
struct ThemeFile {
    name: String,
    #[serde(default)]
    vars: BTreeMap<String, serde_json::Value>,
    colors: BTreeMap<String, serde_json::Value>,
}

impl Theme {
    pub fn color(&self, token: &str) -> Color {
        self.colors.get(token).copied().unwrap_or(Color::Default)
    }

    pub fn fg(&self, token: &str, text: &str) -> String {
        format!("{}{}\x1b[39m", self.color(token).fg_code(), text)
    }

    pub fn bold(&self, text: &str) -> String {
        format!("\x1b[1m{text}\x1b[22m")
    }

    pub fn dim(&self, text: &str) -> String {
        format!("\x1b[2m{text}\x1b[22m")
    }

    pub fn italic(&self, text: &str) -> String {
        format!("\x1b[3m{text}\x1b[23m")
    }

    pub fn load(path: &Path) -> anyhow::Result<Theme> {
        let text = std::fs::read_to_string(path)?;
        Self::parse(&text)
    }

    pub fn parse(text: &str) -> anyhow::Result<Theme> {
        let file: ThemeFile = serde_json::from_str(text)?;
        let resolve_value =
            |v: &serde_json::Value, vars: &BTreeMap<String, serde_json::Value>| -> Option<Color> {
                match v {
                    serde_json::Value::Number(n) => n.as_u64().map(|i| Color::Indexed(i as u8)),
                    serde_json::Value::String(s) => {
                        if let Some(var) = vars.get(s) {
                            match var {
                                serde_json::Value::Number(n) => {
                                    n.as_u64().map(|i| Color::Indexed(i as u8))
                                }
                                serde_json::Value::String(vs) => Color::parse(vs),
                                _ => None,
                            }
                        } else {
                            Color::parse(s)
                        }
                    }
                    _ => None,
                }
            };
        let mut colors = BTreeMap::new();
        for (token, value) in &file.colors {
            if let Some(color) = resolve_value(value, &file.vars) {
                colors.insert(token.clone(), color);
            }
        }
        Ok(Theme {
            name: file.name,
            colors,
        })
    }

    pub fn dark() -> Theme {
        let mut colors = BTreeMap::new();
        let mut set = |k: &str, c: Color| colors.insert(k.to_string(), c);
        set("accent", Color::Rgb(0xcb, 0xa6, 0xf7));
        set("border", Color::Rgb(0x58, 0x5b, 0x70));
        set("borderAccent", Color::Rgb(0xcb, 0xa6, 0xf7));
        set("borderMuted", Color::Rgb(0x31, 0x32, 0x44));
        set("success", Color::Rgb(0xa6, 0xe3, 0xa1));
        set("error", Color::Rgb(0xf3, 0x8b, 0xa8));
        set("warning", Color::Rgb(0xf9, 0xe2, 0xaf));
        set("muted", Color::Rgb(0x7f, 0x84, 0x9c));
        set("dim", Color::Rgb(0x58, 0x5b, 0x70));
        set("text", Color::Rgb(0xcd, 0xd6, 0xf4));
        set("thinkingText", Color::Rgb(0x93, 0x99, 0xb2));
        set("thinkingOff", Color::Rgb(0x58, 0x5b, 0x70));
        set("thinkingMinimal", Color::Rgb(0x6c, 0x70, 0x86));
        set("thinkingLow", Color::Rgb(0x74, 0xc7, 0xec));
        set("thinkingMedium", Color::Rgb(0x89, 0xb4, 0xfa));
        set("thinkingHigh", Color::Rgb(0xb4, 0xbe, 0xfe));
        set("thinkingXhigh", Color::Rgb(0xcb, 0xa6, 0xf7));
        set("thinkingMax", Color::Rgb(0xf5, 0xc2, 0xe7));
        set("bashMode", Color::Rgb(0xa6, 0xe3, 0xa1));
        set("selectedBg", Color::Rgb(0x45, 0x47, 0x5a));
        set("userMessageBg", Color::Rgb(0x31, 0x32, 0x44));
        set("userMessageText", Color::Rgb(0xcd, 0xd6, 0xf4));
        set("toolTitle", Color::Rgb(0x89, 0xb4, 0xfa));
        set("toolOutput", Color::Rgb(0xba, 0xc2, 0xde));
        set("diffAdded", Color::Rgb(0xa6, 0xe3, 0xa1));
        set("diffRemoved", Color::Rgb(0xf3, 0x8b, 0xa8));
        set("mdHeading", Color::Rgb(0xcb, 0xa6, 0xf7));
        set("mdCode", Color::Rgb(0xfa, 0xb3, 0x87));
        set("mdLink", Color::Rgb(0x89, 0xdc, 0xeb));
        set("mdQuote", Color::Rgb(0x93, 0x99, 0xb2));
        Theme {
            name: "dark".into(),
            colors,
        }
    }

    pub fn light() -> Theme {
        let mut colors = BTreeMap::new();
        let mut set = |k: &str, c: Color| colors.insert(k.to_string(), c);
        set("accent", Color::Rgb(0x2a, 0x5b, 0xd7));
        set("border", Color::Indexed(250));
        set("borderAccent", Color::Rgb(0x2a, 0x5b, 0xd7));
        set("borderMuted", Color::Indexed(252));
        set("success", Color::Rgb(0x28, 0x7d, 0x3c));
        set("error", Color::Rgb(0xc0, 0x2c, 0x2c));
        set("warning", Color::Rgb(0x9a, 0x6a, 0x00));
        set("muted", Color::Indexed(243));
        set("dim", Color::Indexed(248));
        set("text", Color::Default);
        set("thinkingText", Color::Indexed(243));
        set("thinkingOff", Color::Rgb(0xb0, 0xb0, 0xb0));
        set("thinkingMinimal", Color::Rgb(0x76, 0x76, 0x76));
        set("thinkingLow", Color::Rgb(0x54, 0x7d, 0xa7));
        set("thinkingMedium", Color::Rgb(0x5a, 0x80, 0x80));
        set("thinkingHigh", Color::Rgb(0x87, 0x5f, 0x87));
        set("thinkingXhigh", Color::Rgb(0x8b, 0x00, 0x8b));
        set("thinkingMax", Color::Rgb(0xaf, 0x00, 0x5f));
        set("bashMode", Color::Rgb(0x58, 0x84, 0x58));
        set("selectedBg", Color::Indexed(254));
        set("userMessageBg", Color::Indexed(255));
        set("userMessageText", Color::Default);
        set("toolTitle", Color::Rgb(0x2a, 0x5b, 0xd7));
        set("toolOutput", Color::Indexed(238));
        set("diffAdded", Color::Rgb(0x28, 0x7d, 0x3c));
        set("diffRemoved", Color::Rgb(0xc0, 0x2c, 0x2c));
        set("mdHeading", Color::Rgb(0x2a, 0x5b, 0xd7));
        set("mdCode", Color::Rgb(0x9a, 0x6a, 0x00));
        set("mdLink", Color::Rgb(0x0f, 0x68, 0xa0));
        set("mdQuote", Color::Indexed(243));
        Theme {
            name: "light".into(),
            colors,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_custom_theme_with_vars() {
        let json = r##"{
            "name": "my-theme",
            "vars": {"primary": "#00aaff", "secondary": 242},
            "colors": {"accent": "primary", "muted": "secondary", "error": "#ff0000", "text": ""}
        }"##;
        let theme = Theme::parse(json).unwrap();
        assert_eq!(theme.color("accent"), Color::Rgb(0, 0xaa, 0xff));
        assert_eq!(theme.color("muted"), Color::Indexed(242));
        assert_eq!(theme.color("error"), Color::Rgb(255, 0, 0));
        assert_eq!(theme.color("text"), Color::Default);
    }

    #[test]
    fn dark_theme_uses_catppuccin_mocha() {
        let dark = Theme::dark();
        assert_eq!(dark.color("text"), Color::Rgb(0xcd, 0xd6, 0xf4));
        assert_eq!(dark.color("accent"), Color::Rgb(0xcb, 0xa6, 0xf7));
        assert_eq!(dark.color("selectedBg"), Color::Rgb(0x45, 0x47, 0x5a));
        assert_eq!(dark.color("success"), Color::Rgb(0xa6, 0xe3, 0xa1));
        assert_eq!(dark.color("warning"), Color::Rgb(0xf9, 0xe2, 0xaf));
        assert_eq!(dark.color("error"), Color::Rgb(0xf3, 0x8b, 0xa8));
        assert_eq!(dark.color("thinkingMax"), Color::Rgb(0xf5, 0xc2, 0xe7));
        assert_eq!(dark.color("bashMode"), Color::Rgb(0xa6, 0xe3, 0xa1));

        // The built-in light theme remains available.
        let light = Theme::light();
        assert_eq!(light.color("thinkingOff"), Color::Rgb(0xb0, 0xb0, 0xb0));
        assert_eq!(light.color("thinkingMinimal"), Color::Rgb(0x76, 0x76, 0x76));
        assert_eq!(light.color("thinkingLow"), Color::Rgb(0x54, 0x7d, 0xa7));
        assert_eq!(light.color("thinkingMedium"), Color::Rgb(0x5a, 0x80, 0x80));
        assert_eq!(light.color("thinkingHigh"), Color::Rgb(0x87, 0x5f, 0x87));
        assert_eq!(light.color("thinkingXhigh"), Color::Rgb(0x8b, 0x00, 0x8b));
        assert_eq!(light.color("thinkingMax"), Color::Rgb(0xaf, 0x00, 0x5f));
        assert_eq!(light.color("bashMode"), Color::Rgb(0x58, 0x84, 0x58));
    }
}
