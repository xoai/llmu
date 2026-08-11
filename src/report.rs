use crate::ansi;
use crate::types::*;
use chrono::{DateTime, Datelike, Utc};
use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Period {
    Day,
    Week,
    Month,
}

impl Period {
    pub fn parse(s: &str) -> Option<Period> {
        match s {
            "day" | "d" => Some(Period::Day),
            "week" | "w" => Some(Period::Week),
            "month" | "m" => Some(Period::Month),
            _ => None,
        }
    }
    pub fn key(&self, dt: DateTime<Utc>) -> String {
        match self {
            Period::Day => dt.format("%Y-%m-%d").to_string(),
            Period::Week => {
                let iw = dt.iso_week();
                format!("{}-W{:02}", iw.year(), iw.week())
            }
            Period::Month => dt.format("%Y-%m").to_string(),
        }
    }
    pub fn label(&self) -> &'static str {
        match self {
            Period::Day => "Day",
            Period::Week => "Week",
            Period::Month => "Month",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Group {
    Provider,
    Model,
    Source,
}

impl Group {
    pub fn parse(s: &str) -> Option<Group> {
        match s {
            "provider" => Some(Group::Provider),
            "model" => Some(Group::Model),
            "source" => Some(Group::Source),
            _ => None,
        }
    }
    pub fn label(&self) -> &'static str {
        match self {
            Group::Provider => "Provider",
            Group::Model => "Model",
            Group::Source => "Src",
        }
    }
    fn value(&self, e: &UsageEvent) -> String {
        match self {
            Group::Provider => e.provider.clone(),
            Group::Model => e.model.clone(),
            Group::Source => e.source.short().into(),
        }
    }
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct Totals {
    pub requests: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub tool_calls: u64,
    pub est_cost_usd: f64,
    pub has_cost: bool,
}

impl Totals {
    fn add(&mut self, e: &UsageEvent) {
        self.requests += e.requests;
        self.input_tokens += e.input_tokens;
        self.output_tokens += e.output_tokens;
        self.cache_read_tokens += e.cache_read_tokens;
        self.cache_write_tokens += e.cache_write_tokens;
        self.tool_calls += e.tool_calls;
        if let Some(c) = e.cost_usd {
            self.est_cost_usd += c;
            self.has_cost = true;
        }
    }
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens + self.output_tokens + self.cache_read_tokens + self.cache_write_tokens
    }
}

#[derive(Debug, Serialize)]
pub struct Row {
    pub keys: Vec<String>,
    #[serde(flatten)]
    pub totals: Totals,
    pub total_tokens: u64,
}

pub fn aggregate(events: &[UsageEvent], period: Period, groups: &[Group]) -> Vec<Row> {
    let mut map: BTreeMap<Vec<String>, Totals> = BTreeMap::new();
    for e in events {
        let mut key = vec![period.key(e.start)];
        key.extend(groups.iter().map(|g| g.value(e)));
        map.entry(key).or_default().add(e);
    }
    map.into_iter()
        .map(|(keys, totals)| Row {
            total_tokens: totals.total_tokens(),
            keys,
            totals,
        })
        .collect()
}

pub fn fmt_int(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

pub fn fmt_cost(t: &Totals) -> String {
    if !t.has_cost {
        "-".into()
    } else if t.est_cost_usd < 0.01 && t.est_cost_usd > 0.0 {
        format!("{:.4}", t.est_cost_usd)
    } else {
        format!("{:.2}", t.est_cost_usd)
    }
}

/// Minimal aligned table: left-align key columns, right-align numbers.
pub fn render_table(period: Period, groups: &[Group], rows: &[Row]) -> String {
    let mut headers: Vec<String> = vec![period.label().into()];
    headers.extend(groups.iter().map(|g| g.label().to_string()));
    let n_keys = headers.len();
    headers.extend(
        [
            "Req", "Input", "Output", "CacheR", "CacheW", "Total", "Tools", "Est$",
        ]
        .iter()
        .map(|s| s.to_string()),
    );

    let mut cells: Vec<Vec<String>> = vec![headers.clone()];
    let mut grand = Totals::default();
    for r in rows {
        let mut row = r.keys.clone();
        row.push(fmt_int(r.totals.requests));
        row.push(fmt_int(r.totals.input_tokens));
        row.push(fmt_int(r.totals.output_tokens));
        row.push(fmt_int(r.totals.cache_read_tokens));
        row.push(fmt_int(r.totals.cache_write_tokens));
        row.push(fmt_int(r.total_tokens));
        row.push(fmt_int(r.totals.tool_calls));
        row.push(fmt_cost(&r.totals));
        cells.push(row);
        grand.requests += r.totals.requests;
        grand.input_tokens += r.totals.input_tokens;
        grand.output_tokens += r.totals.output_tokens;
        grand.cache_read_tokens += r.totals.cache_read_tokens;
        grand.cache_write_tokens += r.totals.cache_write_tokens;
        grand.tool_calls += r.totals.tool_calls;
        grand.est_cost_usd += r.totals.est_cost_usd;
        grand.has_cost |= r.totals.has_cost;
    }
    let mut total_row: Vec<String> = vec!["TOTAL".into()];
    total_row.extend(std::iter::repeat(String::new()).take(n_keys - 1));
    total_row.push(fmt_int(grand.requests));
    total_row.push(fmt_int(grand.input_tokens));
    total_row.push(fmt_int(grand.output_tokens));
    total_row.push(fmt_int(grand.cache_read_tokens));
    total_row.push(fmt_int(grand.cache_write_tokens));
    total_row.push(fmt_int(grand.total_tokens()));
    total_row.push(fmt_int(grand.tool_calls));
    total_row.push(fmt_cost(&grand));
    cells.push(total_row);

    let cols = cells[0].len();
    let mut widths = vec![0usize; cols];
    for row in &cells {
        for (i, c) in row.iter().enumerate() {
            widths[i] = widths[i].max(c.chars().count());
        }
    }
    let mut out = String::new();
    for (ri, row) in cells.iter().enumerate() {
        let mut line = String::new();
        for (i, c) in row.iter().enumerate() {
            let pad = widths[i].saturating_sub(c.chars().count());
            if i < n_keys {
                line.push_str(c);
                line.push_str(&" ".repeat(pad));
            } else {
                line.push_str(&" ".repeat(pad));
                line.push_str(c);
            }
            if i + 1 < cols {
                line.push_str("  ");
            }
        }
        out.push_str(line.trim_end());
        out.push('\n');
        if ri == 0 {
            let sep: Vec<String> = widths.iter().map(|w| "-".repeat(*w)).collect();
            out.push_str(&sep.join("  "));
            out.push('\n');
        }
        if ri + 2 == cells.len() {
            let sep: Vec<String> = widths.iter().map(|w| "-".repeat(*w)).collect();
            out.push_str(&sep.join("  "));
            out.push('\n');
        }
    }
    out
}

/// Rendering style for one quota row: `Command` matches `llmu quota`,
/// `Overview` matches the bare `llmu` landing view. The two views
/// differ only in indentation, resets decoration, and the limit==0
/// column widths; the meter core is shared.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum QuotaStyle {
    Command,
    Overview,
}

/// Render one quota row the way the two quota panels in main.rs print
/// it. `ansi::on()` is env-dependent (identity off a tty), so the
/// returned String is the plain-line form; paint is applied here so
/// the call sites stay identical.
pub fn render_quota(q: &QuotaSnapshot, style: QuotaStyle) -> String {
    render_quota_with_color(q, style, ansi::on())
}

/// Deterministic seam: same rendering as [`render_quota`], but color is
/// injected instead of read from the process environment, so tests get
/// exact bytes without mutating shared env.
fn render_quota_with_color(q: &QuotaSnapshot, style: QuotaStyle, color_enabled: bool) -> String {
    if q.limit > 0.0 {
        let filled = (q.pct() / 5.0).round() as usize;
        let resets = match (q.resets_at, style) {
            (Some(r), QuotaStyle::Command) => r.format("%m-%d %H:%M").to_string(),
            (None, QuotaStyle::Command) => "—".into(),
            (Some(r), QuotaStyle::Overview) => format!("  resets {}", r.format("%m-%d %H:%M")),
            (None, QuotaStyle::Overview) => String::new(),
        };
        let bar = format!(
            "{}{}",
            "#".repeat(filled.min(20)),
            "-".repeat(20usize.saturating_sub(filled))
        );
        let pc = ansi::pct_color(q.pct());
        let used = if q.unit == "%" {
            "—".to_string()
        } else {
            format!(
                "{}/{} {}",
                fmt_int(q.used.max(0.0) as u64),
                fmt_int(q.limit as u64),
                q.unit
            )
        };
        let provider = ansi::paint_when(
            &format!("{:<9}", q.provider),
            ansi::provider_color(&q.provider),
            color_enabled,
        );
        let pct = ansi::paint_when(&format!("{:>6.1}%", q.pct()), pc, color_enabled);
        let meter = ansi::paint_when(&bar, pc, color_enabled);
        let reset = ansi::paint_when(&resets, ansi::DIM, color_enabled);
        match style {
            QuotaStyle::Command => format!(
                "{} {:<28} {:<4} {:>24} {}  [{}]  {}",
                provider, q.plan, q.window, used, pct, meter, reset
            ),
            QuotaStyle::Overview => format!(
                "  {} {:<28} {:<4} {:>24} {}  [{}]{}",
                provider, q.plan, q.window, used, pct, meter, reset
            ),
        }
    } else {
        let used = fmt_int(q.used as u64);
        match style {
            QuotaStyle::Command => format!(
                "{:<10} {:<28} last {}: {} {}",
                q.provider, q.plan, q.window, used, q.unit
            ),
            QuotaStyle::Overview => format!(
                "  {:<8} {:<24} last {}: {} {}",
                q.provider, q.plan, q.window, used, q.unit
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    /// Real `llmu quota` row shape (kimi 7d): limit>0 with resets_at.
    fn with_resets() -> QuotaSnapshot {
        QuotaSnapshot {
            provider: "kimi".into(),
            plan: "Kimi For Coding (standard)".into(),
            window: "7d".into(),
            used: 98.0,
            limit: 100.0,
            unit: "units".into(),
            resets_at: Some(at("2026-08-14T07:49:00Z")),
        }
    }

    /// Real `llmu quota` row shape (codex): limit>0, no resets_at, "%" unit.
    fn without_resets() -> QuotaSnapshot {
        QuotaSnapshot {
            provider: "codex".into(),
            plan: "prolite".into(),
            window: "5h".into(),
            used: 10.0,
            limit: 100.0,
            unit: "%".into(),
            resets_at: None,
        }
    }

    /// Unknown-limit shape: limit == 0 renders used-only, no meter.
    fn limit_zero() -> QuotaSnapshot {
        QuotaSnapshot {
            provider: "codex".into(),
            plan: "prolite".into(),
            window: "7d".into(),
            used: 100.0,
            limit: 0.0,
            unit: "requests".into(),
            resets_at: None,
        }
    }

    /// AC-2 Command variant: exact line bytes for all three quota shapes.
    #[test]
    fn render_quota_command_matches_exact_output() {
        assert_eq!(
            render_quota_with_color(&with_resets(), QuotaStyle::Command, false),
            "kimi      Kimi For Coding (standard)   7d               98/100 units   98.0%  [####################]  08-14 07:49"
        );
        assert_eq!(
            render_quota_with_color(&without_resets(), QuotaStyle::Command, false),
            "codex     prolite                      5h                          —   10.0%  [##------------------]  —"
        );
        assert_eq!(
            render_quota_with_color(&limit_zero(), QuotaStyle::Command, false),
            "codex      prolite                      last 7d: 100 requests"
        );
    }

    /// AC-2 Overview variant: exact line bytes for all three quota shapes.
    #[test]
    fn render_quota_overview_matches_exact_output() {
        assert_eq!(
            render_quota_with_color(&with_resets(), QuotaStyle::Overview, false),
            "  kimi      Kimi For Coding (standard)   7d               98/100 units   98.0%  [####################]  resets 08-14 07:49"
        );
        assert_eq!(
            render_quota_with_color(&without_resets(), QuotaStyle::Overview, false),
            "  codex     prolite                      5h                          —   10.0%  [##------------------]"
        );
        assert_eq!(
            render_quota_with_color(&limit_zero(), QuotaStyle::Overview, false),
            "  codex    prolite                  last 7d: 100 requests"
        );
    }
}
