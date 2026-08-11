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

/// Deterministic aligned table over the normalized balance-history rows
/// (FR-2.7): `from`/`to`/`provider`/`currency` left-aligned, money
/// columns right-aligned with two decimals, header and no totals row.
pub fn render_balance_history(rows: &[BalanceHistoryRow]) -> String {
    let mut cells: Vec<Vec<String>> = vec![[
        "from", "to", "provider", "currency", "opening", "closing", "spent", "funded",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()];
    for r in rows {
        cells.push(vec![
            r.from.to_string(),
            r.to.to_string(),
            r.provider.clone(),
            r.currency.clone(),
            format!("{:.2}", r.opening),
            format!("{:.2}", r.closing),
            format!("{:.2}", r.spent),
            format!("{:.2}", r.funded),
        ]);
    }
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
            if i < 4 {
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
    }
    out
}

// ---------------------------------------------------------------------------
// CSV output (FR-1, Task 8): shared RFC 4180 rendering plus the
// command serializers, all over the same normalized rows the table and
// JSON modes use. UTF-8, LF line endings, one header row, no new
// dependencies.
// ---------------------------------------------------------------------------

/// RFC 4180 field escaping: quote only when the field contains a comma,
/// double quote, CR, or LF; embedded quotes are doubled.
pub fn csv_field(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\r') || s.contains('\n') {
        let mut out = String::with_capacity(s.len() + 2);
        out.push('"');
        for c in s.chars() {
            if c == '"' {
                out.push('"');
            }
            out.push(c);
        }
        out.push('"');
        out
    } else {
        s.to_string()
    }
}

/// One CSV record: escaped fields joined by commas, LF-terminated.
pub fn csv_row(fields: &[String]) -> String {
    let mut out = String::new();
    for (i, f) in fields.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&csv_field(f));
    }
    out.push('\n');
    out
}

/// Shared renderer: exactly one header row plus the given rows. Empty
/// `rows` still emit the header (FR-1.7), so every command prints a
/// parseable CSV even with no data.
pub fn render_csv(header: &[&str], rows: &[Vec<String>]) -> String {
    let header: Vec<String> = header.iter().map(|h| h.to_string()).collect();
    let mut out = csv_row(&header);
    for row in rows {
        out.push_str(&csv_row(row));
    }
    out
}

/// FR-1.4 usage CSV: consumes the same aggregated rows as the table and
/// JSON modes. The group columns use the exact lowercase user-facing
/// names in the user's `--group-by` argument order; numeric cells are
/// machine values (no thousands separators, shortest float round-trip,
/// lowercase booleans). Billed-cost rows are never added (FR-1.4).
pub fn render_usage_csv(groups: &[Group], rows: &[Row]) -> String {
    let mut header: Vec<&str> = vec!["period"];
    header.extend(groups.iter().map(|g| match g {
        Group::Provider => "provider",
        Group::Model => "model",
        Group::Source => "source",
    }));
    header.extend([
        "requests",
        "input_tokens",
        "output_tokens",
        "cache_read_tokens",
        "cache_write_tokens",
        "total_tokens",
        "tool_calls",
        "est_cost_usd",
        "has_cost",
    ]);
    let rows: Vec<Vec<String>> = rows
        .iter()
        .map(|r| {
            let mut cells = r.keys.clone();
            cells.extend([
                r.totals.requests.to_string(),
                r.totals.input_tokens.to_string(),
                r.totals.output_tokens.to_string(),
                r.totals.cache_read_tokens.to_string(),
                r.totals.cache_write_tokens.to_string(),
                r.total_tokens.to_string(),
                r.totals.tool_calls.to_string(),
                r.totals.est_cost_usd.to_string(),
                r.totals.has_cost.to_string(),
            ]);
            cells
        })
        .collect();
    render_csv(&header, &rows)
}

/// FR-1.5 balance CSV: `provider,total,granted,topped_up,currency`.
pub fn render_balance_csv(balances: &[BalanceSnapshot]) -> String {
    let header = ["provider", "total", "granted", "topped_up", "currency"];
    let rows: Vec<Vec<String>> = balances
        .iter()
        .map(|b| {
            vec![
                b.provider.clone(),
                b.total.to_string(),
                b.granted.to_string(),
                b.topped_up.to_string(),
                b.currency.clone(),
            ]
        })
        .collect();
    render_csv(&header, &rows)
}

/// FR-1.6 quota CSV: `provider,plan,window,used,limit,unit,resets_at`.
/// `resets_at` is RFC 3339 when known, empty otherwise.
pub fn render_quota_csv(quotas: &[QuotaSnapshot]) -> String {
    let header = [
        "provider", "plan", "window", "used", "limit", "unit", "resets_at",
    ];
    let rows: Vec<Vec<String>> = quotas
        .iter()
        .map(|q| {
            vec![
                q.provider.clone(),
                q.plan.clone(),
                q.window.clone(),
                q.used.to_string(),
                q.limit.to_string(),
                q.unit.clone(),
                q.resets_at.map(|r| r.to_rfc3339()).unwrap_or_default(),
            ]
        })
        .collect();
    render_csv(&header, &rows)
}

/// FR-2.7 history CSV: `from,to,provider,currency,opening,closing,
/// spent,funded` over the same normalized rows the table and JSON modes
/// use.
pub fn render_balance_history_csv(rows: &[BalanceHistoryRow]) -> String {
    let header = [
        "from", "to", "provider", "currency", "opening", "closing", "spent", "funded",
    ];
    let rows: Vec<Vec<String>> = rows
        .iter()
        .map(|r| {
            vec![
                r.from.to_string(),
                r.to.to_string(),
                r.provider.clone(),
                r.currency.clone(),
                r.opening.to_string(),
                r.closing.to_string(),
                r.spent.to_string(),
                r.funded.to_string(),
            ]
        })
        .collect();
    render_csv(&header, &rows)
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

    // -------------------------------------------------------------------
    // Offline balance history table (FR-2.7): RED contract.
    // -------------------------------------------------------------------

    fn hist_row() -> BalanceHistoryRow {
        BalanceHistoryRow {
            from: chrono::NaiveDate::from_ymd_opt(2026, 8, 1).unwrap(),
            to: chrono::NaiveDate::from_ymd_opt(2026, 8, 2).unwrap(),
            provider: "deepseek".into(),
            currency: "USD".into(),
            opening: 10.0,
            closing: 7.5,
            spent: 2.5,
            funded: 0.0,
        }
    }

    /// Exact table bytes for one decrease row: keys left-aligned, money
    /// right-aligned, header separated, trailing newline.
    #[test]
    fn render_balance_history_matches_exact_output() {
        let expected =
            "from        to          provider  currency  opening  closing  spent  funded\n\
         ----------  ----------  --------  --------  -------  -------  -----  ------\n\
         2026-08-01  2026-08-02  deepseek  USD         10.00     7.50   2.50    0.00\n";
        assert_eq!(render_balance_history(&[hist_row()]), expected);
    }

    /// Every row must appear in table order with its own values.
    #[test]
    fn render_balance_history_lists_rows_in_order() {
        let mut second = hist_row();
        second.provider = "kimi".into();
        second.to = chrono::NaiveDate::from_ymd_opt(2026, 8, 3).unwrap();
        second.opening = 5.0;
        second.closing = 7.0;
        second.spent = 0.0;
        second.funded = 2.0;
        let s = render_balance_history(&[hist_row(), second]);
        let deepseek = s.find("deepseek").unwrap();
        let kimi = s.find("kimi").unwrap();
        assert!(deepseek < kimi, "rows render in given order");
        assert!(s.contains("7.50") && s.contains("2.50") && s.contains("0.00"));
        assert!(s.contains("2.00"));
    }

    // -------------------------------------------------------------------
    // CSV output (FR-1, Task 8): RED contracts for shared RFC 4180
    // rendering and the command serializers.
    // -------------------------------------------------------------------

    fn usage_row(keys: Vec<&str>, est_cost_usd: f64, has_cost: bool) -> Row {
        Row {
            keys: keys.into_iter().map(|k| k.to_string()).collect(),
            total_tokens: 4,
            totals: Totals {
                requests: 2,
                input_tokens: 1_000_000,
                output_tokens: 500,
                cache_read_tokens: 200,
                cache_write_tokens: 0,
                tool_calls: 3,
                est_cost_usd,
                has_cost,
            },
        }
    }

    /// RFC 4180: quote only fields containing comma, quote, CR, or LF;
    /// embedded quotes double.
    #[test]
    fn csv_field_quotes_comma_quote_cr_lf_and_doubles_embedded_quotes() {
        assert_eq!(csv_field("plain"), "plain");
        assert_eq!(csv_field(""), "");
        assert_eq!(csv_field("a,b"), "\"a,b\"");
        assert_eq!(csv_field("a\"b"), "\"a\"\"b\"");
        assert_eq!(csv_field("a\rb"), "\"a\rb\"");
        assert_eq!(csv_field("a\nb"), "\"a\nb\"");
        assert_eq!(csv_field("\"q,c\""), "\"\"\"q,c\"\"\"");
    }

    #[test]
    fn csv_row_joins_fields_and_terminates_with_lf() {
        let row = csv_row(&["a".to_string(), "b,1".to_string(), String::new()]);
        assert_eq!(row, "a,\"b,1\",\n");
    }

    /// FR-1.4: the usage header is `period`, the selected groups in the
    /// user's argument order with exact lowercase names, then the fixed
    /// numeric columns. No billed-cost rows.
    #[test]
    fn usage_csv_uses_exact_lowercase_headers_in_group_order() {
        let groups = [Group::Model, Group::Source];
        let rows = vec![usage_row(
            vec!["2026-08-11", "claude-sonnet-4-5", "local"],
            3.0,
            true,
        )];
        assert_eq!(
            render_usage_csv(&groups, &rows),
            "period,model,source,requests,input_tokens,output_tokens,cache_read_tokens,cache_write_tokens,total_tokens,tool_calls,est_cost_usd,has_cost\n\
             2026-08-11,claude-sonnet-4-5,local,2,1000000,500,200,0,4,3,3,true\n"
        );
    }

    /// FR-1.9: integers unformatted (no thousands separators), finite
    /// floats shortest round-trip Display, booleans lowercase.
    #[test]
    fn usage_csv_machine_values_are_shortest_round_trip() {
        let groups = [Group::Provider];
        let rows = vec![
            usage_row(vec!["2026-08-11", "p1"], 12.34, true),
            usage_row(vec!["2026-08-11", "p2"], 0.01056, true),
            usage_row(vec!["2026-08-11", "p3"], 0.0, false),
        ];
        let csv = render_usage_csv(&groups, &rows);
        assert_eq!(
            csv,
            "period,provider,requests,input_tokens,output_tokens,cache_read_tokens,cache_write_tokens,total_tokens,tool_calls,est_cost_usd,has_cost\n\
             2026-08-11,p1,2,1000000,500,200,0,4,3,12.34,true\n\
             2026-08-11,p2,2,1000000,500,200,0,4,3,0.01056,true\n\
             2026-08-11,p3,2,1000000,500,200,0,4,3,0,false\n"
        );
        assert!(
            !csv.contains("1,000,000"),
            "integers must never carry thousands separators (FR-1.9)"
        );
    }

    /// FR-1.7: empty rows still emit exactly the header.
    #[test]
    fn usage_csv_empty_rows_still_emit_the_header() {
        assert_eq!(
            render_usage_csv(&[Group::Provider], &[]),
            "period,provider,requests,input_tokens,output_tokens,cache_read_tokens,cache_write_tokens,total_tokens,tool_calls,est_cost_usd,has_cost\n"
        );
    }

    /// RFC 4180 escaping reaches the group cells (AS-6).
    #[test]
    fn usage_csv_escapes_group_values_rfc4180() {
        let groups = [Group::Source];
        let rows = vec![usage_row(
            vec!["2026-08-11", "my, \"weird\" model"],
            0.0,
            false,
        )];
        let csv = render_usage_csv(&groups, &rows);
        assert!(
            csv.contains("\"my, \"\"weird\"\" model\""),
            "comma+quote values must be quoted with doubled quotes, got: {csv}"
        );
    }

    /// FR-1.5: balance CSV exact schema, shortest floats, header on empty.
    #[test]
    fn balance_csv_exact_schema_and_shortest_floats() {
        let b = BalanceSnapshot {
            provider: "deepseek".into(),
            currency: "USD".into(),
            total: 12.5,
            granted: 10.0,
            topped_up: 2.5,
        };
        assert_eq!(
            render_balance_csv(&[b]),
            "provider,total,granted,topped_up,currency\n\
             deepseek,12.5,10,2.5,USD\n"
        );
        assert_eq!(
            render_balance_csv(&[]),
            "provider,total,granted,topped_up,currency\n"
        );
    }

    /// FR-1.6: quota CSV exact schema; `resets_at` is RFC 3339 or empty;
    /// header on empty.
    #[test]
    fn quota_csv_rfc3339_or_empty_resets_and_exact_schema() {
        let with = QuotaSnapshot {
            provider: "kimi".into(),
            plan: "Kimi For Coding (standard)".into(),
            window: "7d".into(),
            used: 98.5,
            limit: 100.0,
            unit: "units".into(),
            resets_at: Some(at("2026-08-14T07:49:00Z")),
        };
        let without = QuotaSnapshot {
            provider: "codex".into(),
            plan: "prolite".into(),
            window: "5h".into(),
            used: 10.0,
            limit: 100.0,
            unit: "%".into(),
            resets_at: None,
        };
        assert_eq!(
            render_quota_csv(&[with, without]),
            "provider,plan,window,used,limit,unit,resets_at\n\
             kimi,Kimi For Coding (standard),7d,98.5,100,units,2026-08-14T07:49:00+00:00\n\
             codex,prolite,5h,10,100,%,\n"
        );
        assert_eq!(
            render_quota_csv(&[]),
            "provider,plan,window,used,limit,unit,resets_at\n"
        );
    }

    /// FR-2.7: history CSV consumes the same normalized rows; header on
    /// empty.
    #[test]
    fn history_csv_exact_schema_over_normalized_rows() {
        assert_eq!(
            render_balance_history_csv(&[hist_row()]),
            "from,to,provider,currency,opening,closing,spent,funded\n\
             2026-08-01,2026-08-02,deepseek,USD,10,7.5,2.5,0\n"
        );
        assert_eq!(
            render_balance_history_csv(&[]),
            "from,to,provider,currency,opening,closing,spent,funded\n"
        );
    }
}
