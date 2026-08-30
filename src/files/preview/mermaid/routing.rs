use super::canvas::Canvas;
use super::glyphs::Glyphs;

pub fn horizontal_arrow(
    canvas: &mut Canvas,
    from: usize,
    to: usize,
    y: usize,
    dotted: bool,
    glyphs: Glyphs,
) {
    if from == to {
        return;
    }
    let stroke = if dotted {
        glyphs.dotted
    } else {
        glyphs.horizontal
    };
    if from < to {
        canvas.hline(from, to.saturating_sub(1), y, stroke);
        canvas.put(to, y, glyphs.arrow_right);
    } else {
        canvas.hline(to.saturating_add(1), from, y, stroke);
        canvas.put(to, y, glyphs.arrow_left);
    }
}

pub fn vertical_arrow(
    canvas: &mut Canvas,
    x: usize,
    from: usize,
    to: usize,
    dotted: bool,
    glyphs: Glyphs,
) {
    if from == to {
        return;
    }
    let stroke = if dotted {
        glyphs.dotted
    } else {
        glyphs.vertical
    };
    if from < to {
        canvas.vline(x, from, to.saturating_sub(1), stroke);
        canvas.put(x, to, glyphs.arrow_down);
    } else {
        canvas.vline(x, to.saturating_add(1), from, stroke);
        canvas.put(x, to, glyphs.arrow_up);
    }
}
