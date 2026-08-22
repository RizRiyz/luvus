use std::fmt;
use std::str::FromStr;

use anyhow::{anyhow, bail, Context, Result};
use ratatui::style::Color;
use semver::{Version, VersionReq};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::ui::theme::Theme;

pub const SCHEMA_VERSION: u32 = 1;
pub const MAX_FILE_BYTES: usize = 64 * 1024;
pub const MAX_ID_BYTES: usize = 64;
pub const MAX_DISPLAY_NAME_BYTES: usize = 96;
pub const MAX_DESCRIPTION_BYTES: usize = 512;
pub const MAX_METADATA_BYTES: usize = 128;
pub const MAX_INHERITANCE_DEPTH: usize = 8;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ColorSpec {
    Rgb(u8, u8, u8),
    Indexed(u8),
    Reset,
}

impl ColorSpec {
    pub fn color(self) -> Color {
        match self {
            Self::Rgb(r, g, b) => Color::Rgb(r, g, b),
            Self::Indexed(index) => Color::Indexed(index),
            Self::Reset => Color::Reset,
        }
    }
}

impl fmt::Display for ColorSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rgb(r, g, b) => write!(f, "#{r:02x}{g:02x}{b:02x}"),
            Self::Indexed(index) => write!(f, "ansi({index})"),
            Self::Reset => f.write_str("reset"),
        }
    }
}

impl FromStr for ColorSpec {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        let value = value.trim();
        if value == "reset" {
            return Ok(Self::Reset);
        }
        if let Some(hex) = value.strip_prefix('#') {
            if hex.len() != 6 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                bail!("RGB colors must use #rrggbb");
            }
            return Ok(Self::Rgb(
                u8::from_str_radix(&hex[0..2], 16)?,
                u8::from_str_radix(&hex[2..4], 16)?,
                u8::from_str_radix(&hex[4..6], 16)?,
            ));
        }
        if let Some(index) = value
            .strip_prefix("ansi(")
            .and_then(|rest| rest.strip_suffix(')'))
        {
            if index.is_empty() || !index.bytes().all(|byte| byte.is_ascii_digit()) {
                bail!("indexed colors must use ansi(0) through ansi(255)");
            }
            return Ok(Self::Indexed(
                index
                    .parse::<u8>()
                    .context("ANSI color index must be between 0 and 255")?,
            ));
        }
        bail!("color must be #rrggbb, ansi(0..255), or reset")
    }
}

impl Serialize for ColorSpec {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for ColorSpec {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Appearance {
    #[default]
    Dark,
    Light,
    Terminal,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ThemeColorsFile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crust: Option<ColorSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mantle: Option<ColorSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<ColorSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub surface0: Option<ColorSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub surface1: Option<ColorSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overlay0: Option<ColorSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overlay1: Option<ColorSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtext0: Option<ColorSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtext1: Option<ColorSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<ColorSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accent: Option<ColorSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sel_bg: Option<ColorSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub border: Option<ColorSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub border_focus: Option<ColorSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub green: Option<ColorSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mint: Option<ColorSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub amber: Option<ColorSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coral: Option<ColorSpec>,
}

impl ThemeColorsFile {
    fn entries(&self) -> [(&'static str, Option<ColorSpec>); 18] {
        [
            ("crust", self.crust),
            ("mantle", self.mantle),
            ("base", self.base),
            ("surface0", self.surface0),
            ("surface1", self.surface1),
            ("overlay0", self.overlay0),
            ("overlay1", self.overlay1),
            ("subtext0", self.subtext0),
            ("subtext1", self.subtext1),
            ("text", self.text),
            ("accent", self.accent),
            ("sel_bg", self.sel_bg),
            ("border", self.border),
            ("border_focus", self.border_focus),
            ("green", self.green),
            ("mint", self.mint),
            ("amber", self.amber),
            ("coral", self.coral),
        ]
    }

    fn missing(&self) -> Vec<&'static str> {
        self.entries()
            .into_iter()
            .filter_map(|(name, value)| value.is_none().then_some(name))
            .collect()
    }

    pub fn resolve(&self, parent: Option<&Theme>) -> Result<Theme> {
        if parent.is_none() {
            let missing = self.missing();
            if !missing.is_empty() {
                bail!(
                    "complete theme is missing color{}: {}",
                    if missing.len() == 1 { "" } else { "s" },
                    missing.join(", ")
                );
            }
        }
        let inherited = parent.cloned().unwrap_or_else(Theme::quattro_rally);
        Ok(Theme {
            crust: self.crust.map(ColorSpec::color).unwrap_or(inherited.crust),
            mantle: self
                .mantle
                .map(ColorSpec::color)
                .unwrap_or(inherited.mantle),
            base: self.base.map(ColorSpec::color).unwrap_or(inherited.base),
            surface0: self
                .surface0
                .map(ColorSpec::color)
                .unwrap_or(inherited.surface0),
            surface1: self
                .surface1
                .map(ColorSpec::color)
                .unwrap_or(inherited.surface1),
            overlay0: self
                .overlay0
                .map(ColorSpec::color)
                .unwrap_or(inherited.overlay0),
            overlay1: self
                .overlay1
                .map(ColorSpec::color)
                .unwrap_or(inherited.overlay1),
            subtext0: self
                .subtext0
                .map(ColorSpec::color)
                .unwrap_or(inherited.subtext0),
            subtext1: self
                .subtext1
                .map(ColorSpec::color)
                .unwrap_or(inherited.subtext1),
            text: self.text.map(ColorSpec::color).unwrap_or(inherited.text),
            accent: self
                .accent
                .map(ColorSpec::color)
                .unwrap_or(inherited.accent),
            sel_bg: self
                .sel_bg
                .map(ColorSpec::color)
                .unwrap_or(inherited.sel_bg),
            border: self
                .border
                .map(ColorSpec::color)
                .unwrap_or(inherited.border),
            border_focus: self
                .border_focus
                .map(ColorSpec::color)
                .unwrap_or(inherited.border_focus),
            green: self.green.map(ColorSpec::color).unwrap_or(inherited.green),
            mint: self.mint.map(ColorSpec::color).unwrap_or(inherited.mint),
            amber: self.amber.map(ColorSpec::color).unwrap_or(inherited.amber),
            coral: self.coral.map(ColorSpec::color).unwrap_or(inherited.coral),
        })
    }

    pub fn contains_reset(&self) -> bool {
        self.entries()
            .into_iter()
            .filter_map(|(_, value)| value)
            .any(|value| value == ColorSpec::Reset)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ThemeFile {
    pub schema: u32,
    pub id: String,
    pub display_name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub author: String,
    #[serde(default)]
    pub license: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub requires_luvus: String,
    #[serde(default)]
    pub appearance: Appearance,
    #[serde(default)]
    pub extends: Option<String>,
    pub colors: ThemeColorsFile,
}

impl ThemeFile {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_FILE_BYTES {
            bail!("theme file exceeds the {MAX_FILE_BYTES}-byte limit");
        }
        let text = std::str::from_utf8(bytes).context("theme file must be UTF-8")?;
        let file: Self = toml::from_str(text).context("parse theme TOML")?;
        file.validate_metadata()?;
        Ok(file)
    }

    pub fn validate_metadata(&self) -> Result<()> {
        if self.schema != SCHEMA_VERSION {
            bail!(
                "unsupported theme schema {}; expected {SCHEMA_VERSION}",
                self.schema
            );
        }
        validate_id(&self.id)?;
        bounded_nonempty("display_name", &self.display_name, MAX_DISPLAY_NAME_BYTES)?;
        bounded("description", &self.description, MAX_DESCRIPTION_BYTES)?;
        bounded("author", &self.author, MAX_METADATA_BYTES)?;
        bounded("license", &self.license, MAX_METADATA_BYTES)?;
        bounded("version", &self.version, MAX_METADATA_BYTES)?;
        bounded("requires_luvus", &self.requires_luvus, MAX_METADATA_BYTES)?;
        if let Some(parent) = self.extends.as_deref() {
            validate_id(parent).context("invalid parent theme ID")?;
            if parent == self.id {
                bail!("theme cannot extend itself");
            }
        }
        if !self.version.is_empty() {
            Version::parse(&self.version).context("theme version must be valid semver")?;
        }
        if !self.requires_luvus.is_empty() {
            let requirement = VersionReq::parse(&self.requires_luvus)
                .context("requires_luvus must be a valid semver requirement")?;
            let running = Version::parse(env!("CARGO_PKG_VERSION"))
                .map_err(|error| anyhow!("invalid Luvus package version: {error}"))?;
            if !requirement.matches(&running) {
                bail!(
                    "theme requires Luvus {}, but this is Luvus {}",
                    self.requires_luvus,
                    running
                );
            }
        }
        Ok(())
    }

    pub fn warnings(&self, resolved: &Theme) -> Vec<String> {
        let mut warnings = Vec::new();
        if self.description.trim().is_empty() {
            warnings.push("description is empty".to_string());
        }
        if self.author.trim().is_empty() {
            warnings.push("author is empty".to_string());
        }
        if self.version.trim().is_empty() {
            warnings.push("version is empty".to_string());
        }
        if self.requires_luvus.trim().is_empty() {
            warnings.push("requires_luvus is empty".to_string());
        }
        if self.appearance != Appearance::Terminal && self.colors.contains_reset() {
            warnings.push("reset color used by a non-terminal theme".to_string());
        }
        if let (Some(text), Some(background)) = (rgb(resolved.text), rgb(resolved.mantle)) {
            let ratio = contrast(text, background);
            if ratio < 4.5 {
                warnings.push(format!(
                    "primary text contrast is {ratio:.1}:1; aim for at least 4.5:1"
                ));
            }
        }
        if let (Some(text), Some(background)) = (rgb(resolved.subtext1), rgb(resolved.mantle)) {
            let ratio = contrast(text, background);
            if ratio < 3.0 {
                warnings.push(format!(
                    "secondary text contrast is {ratio:.1}:1; aim for at least 3:1"
                ));
            }
        }
        if self.appearance != Appearance::Terminal {
            let surfaces = [
                resolved.crust,
                resolved.mantle,
                resolved.base,
                resolved.surface0,
                resolved.surface1,
            ];
            if has_duplicate(&surfaces) {
                warnings.push(
                    "two main surfaces are identical, so some UI layers may disappear".to_string(),
                );
            }
            let states = [
                resolved.green,
                resolved.mint,
                resolved.amber,
                resolved.coral,
            ];
            if has_duplicate(&states) {
                warnings.push("two agent state colors are identical".to_string());
            }
        }
        warnings
    }
}

pub fn validate_id(id: &str) -> Result<()> {
    if id.is_empty() {
        bail!("theme ID cannot be empty");
    }
    if id.len() > MAX_ID_BYTES {
        bail!("theme ID cannot exceed {MAX_ID_BYTES} bytes");
    }
    let mut bytes = id.bytes();
    let first = bytes.next().unwrap_or_default();
    if !first.is_ascii_lowercase() && !first.is_ascii_digit() {
        bail!("theme ID must start with a lowercase ASCII letter or number");
    }
    if !bytes.all(|byte| {
        byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-')
    }) {
        bail!(
            "theme ID may contain only lowercase letters, numbers, dots, dashes, and underscores"
        );
    }
    Ok(())
}

fn bounded(name: &str, value: &str, limit: usize) -> Result<()> {
    if value.len() > limit {
        bail!("{name} cannot exceed {limit} bytes");
    }
    if value.chars().any(char::is_control) {
        bail!("{name} cannot contain control characters");
    }
    Ok(())
}

fn bounded_nonempty(name: &str, value: &str, limit: usize) -> Result<()> {
    bounded(name, value, limit)?;
    if value.trim().is_empty() {
        bail!("{name} cannot be empty");
    }
    Ok(())
}

fn has_duplicate<T: PartialEq>(values: &[T]) -> bool {
    values
        .iter()
        .enumerate()
        .any(|(index, value)| values[index + 1..].contains(value))
}

fn rgb(color: Color) -> Option<[u8; 3]> {
    match color {
        Color::Rgb(r, g, b) => Some([r, g, b]),
        _ => None,
    }
}

fn luminance(rgb: [u8; 3]) -> f64 {
    let channel = |value: u8| {
        let value = f64::from(value) / 255.0;
        if value <= 0.03928 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * channel(rgb[0]) + 0.7152 * channel(rgb[1]) + 0.0722 * channel(rgb[2])
}

fn contrast(a: [u8; 3], b: [u8; 3]) -> f64 {
    let (a, b) = (luminance(a), luminance(b));
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn complete_colors() -> &'static str {
        r##"
[colors]
crust = "#000000"
mantle = "#101010"
base = "#202020"
surface0 = "#303030"
surface1 = "#404040"
overlay0 = "#505050"
overlay1 = "#606060"
subtext0 = "#a0a0a0"
subtext1 = "#c0c0c0"
text = "#ffffff"
accent = "ansi(4)"
sel_bg = "#202040"
border = "#505050"
border_focus = "#909090"
green = "ansi(2)"
mint = "ansi(6)"
amber = "ansi(3)"
coral = "ansi(1)"
"##
    }

    #[test]
    fn parses_supported_color_specs() {
        assert_eq!(
            "#12aBcF".parse::<ColorSpec>().unwrap(),
            ColorSpec::Rgb(0x12, 0xab, 0xcf)
        );
        assert_eq!(
            "ansi(255)".parse::<ColorSpec>().unwrap(),
            ColorSpec::Indexed(255)
        );
        assert_eq!("reset".parse::<ColorSpec>().unwrap(), ColorSpec::Reset);
        for invalid in ["red", "#fff", "#gg0000", "ansi(-1)", "ansi(256)", "ansi()"] {
            assert!(invalid.parse::<ColorSpec>().is_err(), "{invalid}");
        }
    }

    #[test]
    fn complete_theme_parses_and_resolves() {
        let source = format!(
            "schema = 1\nid = \"test-theme\"\ndisplay_name = \"Test Theme\"\nappearance = \"dark\"\n{}",
            complete_colors()
        );
        let file = ThemeFile::parse(source.as_bytes()).unwrap();
        let theme = file.colors.resolve(None).unwrap();
        assert_eq!(theme.accent, Color::Indexed(4));
    }

    #[test]
    fn partial_theme_requires_a_parent() {
        let source = br##"schema = 1
id = "child"
display_name = "Child"
[colors]
accent = "#123456"
"##;
        let file = ThemeFile::parse(source).unwrap();
        assert!(file.colors.resolve(None).is_err());
        assert_eq!(
            file.colors.resolve(Some(&Theme::noir())).unwrap().accent,
            Color::Rgb(0x12, 0x34, 0x56)
        );
    }

    #[test]
    fn website_schema_matches_the_runtime_roles() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("website/src/lib/theme-schema.ts");
        let source = std::fs::read_to_string(path).unwrap();
        let fields = source
            .split("export const WARM_COPPER")
            .next()
            .expect("field declaration precedes examples");
        let runtime_fields = ThemeColorsFile::default().entries();
        assert_eq!(fields.matches("id: '").count(), runtime_fields.len());
        for (field, _) in runtime_fields {
            assert!(
                fields.contains(&format!("id: '{field}'")),
                "website schema is missing {field}"
            );
        }
    }

    #[test]
    fn website_maker_emits_the_012_contract_without_a_license_prompt() {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("website/src/pages/themes.astro");
        let source = std::fs::read_to_string(path).unwrap();
        let schema = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("website/src/lib/theme-schema.ts"),
        )
        .unwrap();
        assert!(schema.contains("THEME_MIN_LUVUS = '0.12.0'"));
        assert!(source.contains("`requires_luvus = \">=${THEME_MIN_LUVUS}\"`"));
        assert!(!source.contains("theme-license"));
        assert!(source
            .contains("https://github.com/RizRiyz/luvus/blob/main/community/themes/README.md"));
    }

    #[test]
    fn rejects_unknown_fields_and_unsafe_ids() {
        for id in ["", "Upper", "../escape", "with/slash", "two words"] {
            assert!(validate_id(id).is_err(), "{id:?}");
        }
        let source = br##"schema = 1
id = "safe"
display_name = "Safe"
script = "nope"
[colors]
accent = "#123456"
"##;
        assert!(ThemeFile::parse(source).is_err());
    }
}
