//! Per-render opaque rectangles and bounded cell-grid subtraction. This is
//! client-local presentation state, not terminal/runtime state.
use super::{clip_placement, source_extent, Clipped, HostPlacementKey, ScreenPlacement};
use ratatui::layout::Rect;

pub(crate) const MAX_EXTRA_CROP_PIECES: usize = 32;
const MAX_REGIONS: usize = 64;

#[derive(Debug, Default, Clone)]
pub(crate) struct Occlusion {
    regions: Vec<Rect>,
}

impl Occlusion {
    pub(crate) fn cover(&mut self, rect: Rect) {
        if rect.is_empty() || self.regions.iter().any(|r| r.intersection(rect) == rect) {
            return;
        }
        self.regions.retain(|r| rect.intersection(*r) != *r);
        if self.regions.len() == MAX_REGIONS {
            // Unusual overlay storms fail closed with one enclosing region.
            // Normal renderers produce only a handful of opaque surfaces.
            let enclosing = self.regions.iter().fold(rect, |a, b| a.union(*b));
            self.regions.clear();
            self.regions.push(enclosing);
        } else {
            self.regions.push(rect);
        }
    }

    pub(crate) fn touches(&self, rect: Rect) -> bool {
        self.regions.iter().any(|r| r.intersects(rect))
    }

    fn pieces(&self, rect: Rect, limit: usize) -> Option<Vec<Rect>> {
        let mut pieces = vec![rect];
        for cover in &self.regions {
            let mut next = Vec::with_capacity((limit + 1).min(MAX_EXTRA_CROP_PIECES + 2));
            for piece in pieces {
                let cut = piece.intersection(*cover);
                if cut.is_empty() {
                    next.push(piece);
                } else {
                    for (left, top, right, bottom) in [
                        (piece.x, piece.y, piece.right(), cut.y),
                        (piece.x, cut.bottom(), piece.right(), piece.bottom()),
                        (piece.x, cut.y, cut.x, cut.bottom()),
                        (cut.right(), cut.y, piece.right(), cut.bottom()),
                    ] {
                        if left < right && top < bottom {
                            next.push(Rect::new(left, top, right - left, bottom - top));
                        }
                    }
                }
                if next.len() > limit {
                    return None;
                }
            }
            pieces = next;
            if pieces.is_empty() {
                break;
            }
        }
        Some(pieces)
    }
}

type FramePiece<'a> = (HostPlacementKey, Clipped, &'a ScreenPlacement);

/// At most N + 32 output rectangles and 64 regions. If the frame budget is
/// exceeded, hide every intersecting placement, preserving untouched images.
/// Re-run only the cheap intersection check, not unbounded subdivision.
pub(super) fn frame_pieces<'a>(
    placements: &'a [ScreenPlacement],
    occlusion: &Occlusion,
) -> Vec<FramePiece<'a>> {
    let mut extra = MAX_EXTRA_CROP_PIECES;
    let mut out = Vec::with_capacity(placements.len().saturating_add(MAX_EXTRA_CROP_PIECES));
    for p in placements {
        let Some((whole, _)) = clip_placement(p) else {
            continue;
        };
        let rect = Rect::new(whole.x, whole.y, whole.cols as u16, whole.rows as u16);
        if !occlusion.touches(rect) {
            out.push((key(p, None), whole, p));
            continue;
        }
        let Some(pieces) = occlusion.pieces(rect, extra + 1) else {
            return placements
                .iter()
                .filter_map(|p| {
                    let (whole, _) = clip_placement(p)?;
                    let rect = Rect::new(whole.x, whole.y, whole.cols as u16, whole.rows as u16);
                    (!occlusion.touches(rect)).then_some((key(p, None), whole, p))
                })
                .collect();
        };
        extra -= pieces.len().saturating_sub(1);
        for piece in pieces {
            if let Some(clipped) = crop_piece(p, piece) {
                // Relative geometry is the identity: unchanged fragments keep
                // IDs, changed fragments retire before any ID can be reused.
                let relative = Rect::new(
                    piece.x - whole.x,
                    piece.y - whole.y,
                    piece.width,
                    piece.height,
                );
                out.push((key(p, Some(relative)), clipped, p));
            }
        }
    }
    out
}

fn key(p: &ScreenPlacement, piece: Option<Rect>) -> HostPlacementKey {
    HostPlacementKey {
        pane_id: p.pane_id,
        image_id: p.visible.image_id,
        placement_id: p.visible.placement_id,
        piece,
    }
}

/// Absolute display-pixel boundaries map to absolute source boundaries. Using
/// independent widths would round each sliver differently and create seams.
fn span(
    start: u32,
    cells: u32,
    total: u32,
    cell_px: u32,
    offset: u32,
    source: u32,
) -> Option<(u32, u32)> {
    let display = u64::from(total) * u64::from(cell_px);
    let offset = u64::from(offset).min(display.saturating_sub(1));
    let edge = |cell: u32| {
        let px = (u64::from(cell) * u64::from(cell_px))
            .min(display)
            .max(offset);
        ((px - offset) * u64::from(source) / display.saturating_sub(offset).max(1)) as u32
    };
    let first = edge(start);
    let last = edge(start + cells);
    // Integer source coordinates cannot represent a subpixel cut in an
    // upscaled image. Keep at least its one sampled pixel, never encode zero
    // (which Kitty interprets as the entire remaining source rectangle).
    (source > 0).then_some((
        first.min(source.saturating_sub(1)),
        last.saturating_sub(first).max(1),
    ))
}

pub(super) fn crop_piece(p: &ScreenPlacement, rect: Rect) -> Option<Clipped> {
    let v = &p.visible;
    let left = (i64::from(rect.x) - i64::from(p.area_x) - i64::from(v.viewport_col)) as u32;
    let top = (i64::from(rect.y) - i64::from(p.area_y) - i64::from(v.viewport_row)) as u32;
    let (x, width) = span(
        left,
        u32::from(rect.width),
        v.grid_cols,
        p.cell_width_px,
        v.x_offset,
        source_extent(v.image_width, v.source_x, v.source_width),
    )?;
    let (y, height) = span(
        top,
        u32::from(rect.height),
        v.grid_rows,
        p.cell_height_px,
        v.y_offset,
        source_extent(v.image_height, v.source_y, v.source_height),
    )?;
    Some(Clipped {
        x: rect.x,
        y: rect.y,
        cols: u32::from(rect.width),
        rows: u32::from(rect.height),
        source_x: v.source_x + x,
        source_y: v.source_y + y,
        source_width: width,
        source_height: height,
        x_offset: if left == 0 { v.x_offset } else { 0 },
        y_offset: if top == 0 { v.y_offset } else { 0 },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn region_inventory_is_bounded_and_never_loses_coverage() {
        let mut occlusion = Occlusion::default();
        for i in 0..200 {
            occlusion.cover(Rect::new(i * 2, 2, 1, 1));
            assert!(occlusion.regions.len() <= MAX_REGIONS);
        }
        for i in 0..200 {
            assert!(occlusion.touches(Rect::new(i * 2, 2, 1, 1)));
        }
    }
}
