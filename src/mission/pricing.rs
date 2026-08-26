//! Mission Control cost model (docs/54 §5): the model price + context-window
//! tables and the cost/context estimates built from them. Pure and
//! self-contained — **cost is always an estimate**, and it is overridable per
//! model via `config.mission_pricing`.

/// Context-window fraction at/above which agents auto-compact (docs/54 §5). Rows
/// near this line are flagged "compacts soon". Matches the orch gate.
pub const COMPACT_AT: f32 = 0.85;

/// Rough USD price per **million** tokens as `(input, output, cache)` for `model`
/// (substring match on the model id). `None` for an unknown model, so its cost is
/// shown as "—" rather than a wrong number. Estimates only; overridable via config
/// (MC-5). Kept deliberately conservative and easy to eyeball. Anthropic rows
/// track the current first-party API rates (2026-08): Fable/Mythos 5 at
/// $10/$50, Opus 4.6–5 at $5/$25, Haiku 4.5 at $1/$5, with cache at the
/// standard 0.1× input read rate.
pub fn model_price(model: &str) -> Option<(f64, f64, f64)> {
    let m = model.to_lowercase();
    let p = if m.contains("fable") || m.contains("mythos") {
        (10.0, 50.0, 1.0)
    } else if m.contains("opus") {
        (5.0, 25.0, 0.5)
    } else if m.contains("sonnet") {
        (3.0, 15.0, 0.3)
    } else if m.contains("haiku") {
        (1.0, 5.0, 0.1)
    } else if m.contains("gpt-4o") || m.contains("gpt-4.1") || m.contains("gpt-5") {
        (2.5, 10.0, 1.25)
    } else if m.contains("o1") || m.contains("o3") {
        (15.0, 60.0, 7.5)
    } else {
        return None;
    };
    Some(p)
}

/// The model's context window in tokens (substring match); a safe default when
/// unknown, so the context bar still shows something reasonable.
pub fn model_window(model: &str) -> u64 {
    let m = model.to_lowercase();
    if m.contains("fable") || m.contains("mythos") {
        // Claude Fable/Mythos 5 run a 1M window always (it is also the default),
        // so the 200k base + inference in `context_frac` would read a sub-200k
        // session as 5× fuller than it is.
        1_000_000
    } else if m.contains("gpt") || m.contains("o1") || m.contains("o3") {
        128_000
    } else {
        200_000 // Claude default (see `context_frac` for the 1M extended window)
    }
}

/// Estimated USD cost of a session (`None` for an unknown/unpriced model).
pub fn estimate_cost(model: &str, tokens_in: u64, tokens_out: u64, cache: u64) -> Option<f64> {
    let (pin, pout, pcache) = model_price(model)?;
    Some((tokens_in as f64 * pin + tokens_out as f64 * pout + cache as f64 * pcache) / 1_000_000.0)
}

/// Like [`estimate_cost`] but a user `overrides` map (model-id substring →
/// `[input, output, cache]` per million) wins over the built-in table (docs/54
/// MC-5). Empty overrides ⇒ identical to [`estimate_cost`].
pub fn estimate_cost_with(
    model: &str,
    tokens_in: u64,
    tokens_out: u64,
    cache: u64,
    overrides: &std::collections::HashMap<String, [f64; 3]>,
) -> Option<f64> {
    let m = model.to_lowercase();
    let (pin, pout, pcache) = overrides
        .iter()
        .find(|(k, _)| m.contains(k.to_lowercase().as_str()))
        .map(|(_, p)| (p[0], p[1], p[2]))
        .or_else(|| model_price(model))?;
    Some((tokens_in as f64 * pin + tokens_out as f64 * pout + cache as f64 * pcache) / 1_000_000.0)
}

/// Fraction (0..1) of the model's context window that `ctx_tokens` fills.
///
/// Claude runs a 200k window by default but a **1M** window in extended mode, and
/// the model id doesn't say which. So the window is *inferred from the data*: a
/// session whose context already exceeds Claude's 200k base must be on the 1M
/// window (you can't hold more context than the window), which keeps a near-full
/// 978k session reading as ~98% rather than clamping every large session to 100%.
pub fn context_frac(model: &str, ctx_tokens: u64) -> f32 {
    let base = model_window(model);
    let window = if base == 200_000 && ctx_tokens > 200_000 {
        1_000_000
    } else {
        base
    };
    (ctx_tokens as f64 / window.max(1) as f64).clamp(0.0, 1.0) as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pricing_context_and_overrides() {
        // Known models priced; unknown ones return None (shown as "—").
        assert!(model_price("claude-opus-4-8").is_some());
        assert!(model_price("some-unknown-model").is_none());
        // 1M input tokens of sonnet at $3/M = $3.00.
        assert_eq!(estimate_cost("claude-sonnet-4", 1_000_000, 0, 0), Some(3.0));
        assert_eq!(estimate_cost("mystery", 1000, 1000, 0), None);
        // Fable/Mythos 5 are priced ($10/M input) rather than falling through
        // to the unknown-model "—".
        assert_eq!(estimate_cost("claude-fable-5", 1_000_000, 0, 0), Some(10.0));
        assert!(model_price("claude-mythos-5").is_some());
        // Context fraction: 100k of a 200k Claude window = 0.5.
        assert!((context_frac("claude-opus", 100_000) - 0.5).abs() < 1e-6);
        // A session past 200k is inferred to be on the 1M window: a real ~978k
        // context reads as ~98%, not clamped to 100% against a wrong 200k window.
        assert_eq!(
            (context_frac("claude-opus-4-8", 978_000) * 100.0).round() as u32,
            98
        );
        assert_eq!(
            (context_frac("claude-opus", 150_000) * 100.0).round() as u32,
            75
        );
        // Over even the 1M window clamps to 1.0.
        assert_eq!(context_frac("claude-opus", 9_999_999_999), 1.0);
        // Fable's window is 1M always: 100k reads as 10%, not 50% of an assumed
        // 200k base.
        assert!((context_frac("claude-fable-5", 100_000) - 0.1).abs() < 1e-6);
        // A user override wins over the built-in table.
        let mut ov = std::collections::HashMap::new();
        ov.insert("opus".to_string(), [10.0, 20.0, 1.0]);
        assert_eq!(
            estimate_cost_with("claude-opus-4-8", 1_000_000, 0, 0, &ov),
            Some(10.0)
        );
        // Empty overrides == the default estimate.
        let empty = std::collections::HashMap::new();
        assert_eq!(
            estimate_cost_with("claude-sonnet-4", 1_000_000, 0, 0, &empty),
            estimate_cost("claude-sonnet-4", 1_000_000, 0, 0),
        );
    }

    #[test]
    fn compaction_threshold_flags_high_context() {
        // A near-full context trips the compaction line; a half-full one doesn't.
        let full = context_frac("claude-opus", 190_000); // 0.95
        let half = context_frac("claude-opus", 100_000); // 0.50
        assert!(full >= COMPACT_AT, "{full} should flag");
        assert!(half < COMPACT_AT, "{half} should not");
    }
}
