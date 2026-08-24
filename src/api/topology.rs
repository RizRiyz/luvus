use ratatui::layout::Rect;
use serde_json::Value;

use crate::api::error::{ApiError, ApiResult};
use crate::layout::{Dir, LayoutTree};

pub const MAX_LAYOUT_DEPTH: usize = 64;
pub const MAX_WORKSPACE_MOVE_BLOCK: usize = 256;
pub const MAX_EVENT_WAIT_S: u64 = 3600;
pub const LOGICAL_LAYOUT_WIDTH: u16 = 10_000;
pub const LOGICAL_LAYOUT_HEIGHT: u16 = 10_000;

pub fn logical_area() -> Rect {
    Rect::new(0, 0, LOGICAL_LAYOUT_WIDTH, LOGICAL_LAYOUT_HEIGHT)
}

pub fn direction(params: &Value) -> ApiResult<Dir> {
    match params.get("direction").and_then(Value::as_str) {
        Some("left") => Ok(Dir::Left),
        Some("right") => Ok(Dir::Right),
        Some("up") => Ok(Dir::Up),
        Some("down") => Ok(Dir::Down),
        _ => Err(ApiError::new(
            "invalid_request",
            "direction must be left, right, up, or down",
        )),
    }
}

pub fn split_path(params: &Value) -> ApiResult<Vec<bool>> {
    let path = params
        .get("path")
        .and_then(Value::as_array)
        .ok_or_else(|| ApiError::new("invalid_request", "path must be an array of a/b steps"))?;
    if path.len() > MAX_LAYOUT_DEPTH {
        return Err(ApiError::new("limit_exceeded", "layout path is too deep"));
    }
    path.iter()
        .map(|step| match step.as_str() {
            Some("a") => Ok(false),
            Some("b") => Ok(true),
            _ => Err(ApiError::new(
                "invalid_request",
                "layout path steps must be a or b",
            )),
        })
        .collect()
}

pub fn parse_tree(value: &Value) -> ApiResult<LayoutTree> {
    let tree: LayoutTree = serde_json::from_value(value.clone())
        .map_err(|_| ApiError::new("invalid_request", "tree is not a valid layout tree"))?;
    validate_tree(&tree, 0)?;
    Ok(tree)
}

fn validate_tree(tree: &LayoutTree, depth: usize) -> ApiResult<()> {
    if depth > MAX_LAYOUT_DEPTH {
        return Err(ApiError::new("limit_exceeded", "layout tree is too deep"));
    }
    match tree {
        LayoutTree::Leaf(_) => Ok(()),
        LayoutTree::Split { axis, ratio, a, b } => {
            if *axis > 1 {
                return Err(ApiError::new(
                    "invalid_request",
                    "layout axis must be 0 (columns) or 1 (rows)",
                ));
            }
            if !ratio.is_finite() || !(0.0..=1.0).contains(ratio) {
                return Err(ApiError::new(
                    "invalid_request",
                    "layout ratios must be finite values from 0 to 1",
                ));
            }
            validate_tree(a, depth + 1)?;
            validate_tree(b, depth + 1)
        }
    }
}
