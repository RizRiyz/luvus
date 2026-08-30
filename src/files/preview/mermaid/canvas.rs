use unicode_width::UnicodeWidthChar;

/// A bounded terminal-cell canvas. Wide characters reserve their continuation
/// cells so diagram geometry remains aligned for multilingual labels.
pub struct Canvas {
    width: usize,
    cells: Vec<Vec<char>>,
}

impl Canvas {
    pub fn new(width: usize, height: usize) -> Self {
        Self {
            width,
            cells: vec![vec![' '; width]; height],
        }
    }

    pub fn height(&self) -> usize {
        self.cells.len()
    }

    pub fn put(&mut self, x: usize, y: usize, ch: char) {
        if let Some(cell) = self.cells.get_mut(y).and_then(|row| row.get_mut(x)) {
            *cell = ch;
        }
    }

    pub fn write(&mut self, mut x: usize, y: usize, text: &str) {
        for ch in text.chars() {
            let width = ch.width().unwrap_or(0);
            if width == 0 {
                continue;
            }
            if x + width > self.width {
                break;
            }
            self.put(x, y, ch);
            for continuation in 1..width {
                // A terminal renders this column as part of the wide glyph.
                // Keep it reserved in geometry but omit it from the final string.
                self.put(x + continuation, y, '\0');
            }
            x += width;
        }
    }

    pub fn hline(&mut self, x0: usize, x1: usize, y: usize, ch: char) {
        for x in x0.min(x1)..=x0.max(x1).min(self.width.saturating_sub(1)) {
            self.put(x, y, ch);
        }
    }

    pub fn vline(&mut self, x: usize, y0: usize, y1: usize, ch: char) {
        for y in y0.min(y1)..=y0.max(y1).min(self.height().saturating_sub(1)) {
            self.put(x, y, ch);
        }
    }

    pub fn into_lines(self) -> Vec<String> {
        self.cells
            .into_iter()
            .map(|row| {
                row.into_iter()
                    .filter(|ch| *ch != '\0')
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_width::UnicodeWidthStr;

    #[test]
    fn wide_glyph_continuations_do_not_add_output_columns() {
        let mut canvas = Canvas::new(4, 1);
        canvas.write(0, 0, "你好");

        let line = canvas.into_lines().remove(0);
        assert_eq!(line, "你好");
        assert_eq!(line.width(), 4);
    }
}
