//! What a call costs at the API's list prices, per model. A cache write
//! costs 1.25× input for the 5-minute cache and 2× for the hour; a cache
//! read's rate is each model's own (0.05× input on Opus 5.5, 0.1× on most).
//! Fast mode costs twice as much. Checked against the `cost-state` totals
//! Claude Code writes into its transcripts.

/// Dollars per token.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Price {
    pub input: f64,
    pub output: f64,
    pub read: f64,
}

impl Price {
    pub fn write_5m(&self) -> f64 {
        self.input * 1.25
    }
    pub fn write_1h(&self) -> f64 {
        self.input * 2.0
    }
}

/// Dollars per million tokens: input, output, cache read. The most specific
/// id first; the first prefix that matches wins.
const TABLE: [(&str, f64, f64, f64); 14] = [
    ("claude-fable-5-1", 10.0, 50.0, 0.25),
    ("claude-mythos-5-1", 10.0, 50.0, 0.25),
    ("claude-fable-5", 10.0, 50.0, 1.0),
    ("claude-mythos-5", 10.0, 50.0, 1.0),
    ("claude-opus-5-5", 4.0, 20.0, 0.20),
    ("claude-opus-5", 5.0, 25.0, 0.50),
    ("claude-opus-4-8", 5.0, 25.0, 0.50),
    ("claude-opus-4-7", 5.0, 25.0, 0.50),
    ("claude-opus-4-6", 5.0, 25.0, 0.50),
    ("claude-opus-4-5", 5.0, 25.0, 0.50),
    ("claude-sonnet-5", 2.0, 10.0, 0.20),
    ("claude-sonnet-4", 3.0, 15.0, 0.30),
    ("claude-haiku-4-5", 1.0, 5.0, 0.10),
    ("claude-opus-4", 15.0, 75.0, 1.50),
];

/// The price of a call to `model`. Claude models toomux doesn't know are
/// priced as Opus 5.5, the default; another provider's (a local model behind
/// a gateway) cost nothing here.
pub fn of(model: &str, fast: bool) -> Price {
    if !model.is_empty() && !model.starts_with("claude-") && !model.starts_with('<') {
        return Price {
            input: 0.0,
            output: 0.0,
            read: 0.0,
        };
    }
    let (_, i, o, r) = TABLE
        .iter()
        .find(|(id, ..)| model.starts_with(id))
        .copied()
        .unwrap_or(TABLE[4]);
    let k = if fast { 2.0 } else { 1.0 } / 1e6;
    Price {
        input: i * k,
        output: o * k,
        read: r * k,
    }
}

/// "opus 5.5" for `claude-opus-5-5`, "haiku 4.5" for `claude-haiku-4-5-20251001`.
pub fn name(model: &str) -> String {
    let m = model.strip_prefix("claude-").unwrap_or(model);
    let mut parts: Vec<&str> = m.split('-').collect();
    // A dated snapshot's date.
    if parts
        .last()
        .is_some_and(|p| p.len() == 8 && p.chars().all(|c| c.is_ascii_digit()))
    {
        parts.pop();
    }
    match parts.split_first() {
        Some((family, version)) if !version.is_empty() => format!("{family} {}", version.join(".")),
        _ => m.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_session_costs_what_claude_code_says() {
        // Session f2242663's own cost-state: opus 5.5, $10.410496, all of its
        // cache writes for the hour; haiku 4.5, $0.065022, for five minutes.
        let p = of("claude-opus-5-5", false);
        let opus = 4_810.0 * p.input
            + 140_187.0 * p.output
            + 26_535_740.0 * p.read
            + 285_046.0 * p.write_1h();
        assert!((opus - 10.410496).abs() < 1e-6, "{opus}");
        let h = of("claude-haiku-4-5-20251001", false);
        let haiku =
            11_203.0 * h.input + 1_756.0 * h.output + 93_452.0 * h.read + 28_555.0 * h.write_5m();
        assert!((haiku - 0.06502195).abs() < 1e-6, "{haiku}");
    }

    fn per_million(v: f64, dollars: f64) -> bool {
        (v * 1e6 - dollars).abs() < 1e-9
    }

    #[test]
    fn the_most_specific_model_wins() {
        assert!(per_million(of("claude-opus-5-5", false).read, 0.20));
        assert!(per_million(of("claude-opus-5", false).read, 0.50));
        assert!(per_million(of("claude-fable-5-1", false).read, 0.25));
        assert!(per_million(of("claude-fable-5", false).read, 1.0));
        assert!(per_million(of("claude-opus-5-5", true).output, 40.0));
        assert_eq!(of("<synthetic>", false), of("claude-opus-5-5", false));
        assert_eq!(of("qwen3-coder:30b-a3b", false).output, 0.0);
        assert_eq!(name("claude-haiku-4-5-20251001"), "haiku 4.5");
        assert_eq!(name("claude-opus-5-5"), "opus 5.5");
    }
}
