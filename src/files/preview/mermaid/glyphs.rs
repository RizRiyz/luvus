#[derive(Clone, Copy)]
pub struct Glyphs {
    pub horizontal: char,
    pub vertical: char,
    pub top_left: char,
    pub top_right: char,
    pub bottom_left: char,
    pub bottom_right: char,
    pub arrow_right: char,
    pub arrow_left: char,
    pub arrow_down: char,
    pub arrow_up: char,
    pub dotted: char,
}

impl Glyphs {
    pub fn for_ascii(ascii: bool) -> Self {
        if ascii {
            Self {
                horizontal: '-',
                vertical: '|',
                top_left: '+',
                top_right: '+',
                bottom_left: '+',
                bottom_right: '+',
                arrow_right: '>',
                arrow_left: '<',
                arrow_down: 'v',
                arrow_up: '^',
                dotted: '.',
            }
        } else {
            Self {
                horizontal: '─',
                vertical: '│',
                top_left: '┌',
                top_right: '┐',
                bottom_left: '└',
                bottom_right: '┘',
                arrow_right: '▶',
                arrow_left: '◀',
                arrow_down: '▼',
                arrow_up: '▲',
                dotted: '┄',
            }
        }
    }
}
