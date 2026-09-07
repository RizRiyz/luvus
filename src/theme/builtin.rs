use std::sync::LazyLock;

use super::format::ThemeFile;
use crate::ui::theme::Theme;

const SOURCES: &[&str] = &[
    include_str!("baitong/baitong.toml"),
    include_str!("paper/paper.toml"),
    include_str!("papercolor/papercolor.toml"),
    include_str!("rose-pine/rose-pine.toml"),
    include_str!("rose-pine/rose-pine-moon.toml"),
    include_str!("rose-pine/rose-pine-dawn.toml"),
    include_str!("tokyo-night/tokyo-night.toml"),
];

struct BuiltinTheme {
    file: ThemeFile,
    theme: Theme,
}

static THEMES: LazyLock<Vec<BuiltinTheme>> = LazyLock::new(|| {
    SOURCES
        .iter()
        .map(|source| {
            let file = ThemeFile::parse(source.as_bytes())
                .expect("bundled theme files must pass schema validation");
            let theme = file
                .colors
                .resolve(None)
                .expect("bundled themes must define every semantic color");
            BuiltinTheme { file, theme }
        })
        .collect()
});

pub fn file(id: &str) -> Option<&'static ThemeFile> {
    THEMES
        .iter()
        .find(|theme| theme.file.id == id)
        .map(|theme| &theme.file)
}

pub fn theme(id: &str) -> Option<Theme> {
    THEMES
        .iter()
        .find(|theme| theme.file.id == id)
        .map(|theme| theme.theme.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_theme_files_are_valid_and_unique() {
        let mut ids = std::collections::HashSet::new();
        for bundled in THEMES.iter() {
            let file = &bundled.file;
            assert!(
                ids.insert(file.id.clone()),
                "duplicate theme ID {}",
                file.id
            );
        }
        assert_eq!(ids.len(), SOURCES.len());
    }
}
