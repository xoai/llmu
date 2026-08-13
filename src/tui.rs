//! Live dashboard (`llmu tui` / `llmu watch`).
//!
//! Two refresh cadences run on a background worker thread so the UI
//! never blocks:
//!   - LOCAL tick (default 3s): re-parses Claude Code, Codex, Gemini,
//!     and Qwen local usage records — no network requests.
//!   - NETWORK tick (default 60s): full fetch including provider usage
//!     APIs, quotas, and balances. Kept slow deliberately — the Claude
//!     oauth/usage endpoint rate-limits aggressively.
//!
//! Keys: q quit • d/w/m period • r force network refresh • p pause.

use crate::config::Config;
use crate::providers::Provider;
use crate::report::{self, Group, Period};
use crate::types::*;
use anyhow::{Context as _, Result};
use chrono::{DateTime, Duration as CDuration, DurationRound, Utc};
use crossterm::{
    event::{self, Event, KeyCode},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{prelude::*, widgets::*};
use std::collections::BTreeMap;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

#[derive(Clone, Default)]
pub struct Dashboard {
    pub events: Vec<UsageEvent>,
    pub quotas: Vec<QuotaSnapshot>,
    pub balances: Vec<BalanceSnapshot>,
    pub billed: Vec<BilledCost>,
    pub window_label: String,
}

struct Snap {
    d: Dashboard,
    at: DateTime<Utc>,
    net: bool, // was this a full network refresh?
}

/// Raw-cache bypass lifetime for the TUI (FR-3.2): a one-shot `--fresh`
/// applies only to the initial full network fetch; each `r` keypress
/// bypasses exactly its next full network fetch and then clears.
/// Local-only ticks never consume the state (they do not call `take`),
/// and scheduled network ticks otherwise honor the TTL. Pure — the
/// freshness decision is testable without a terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FreshState {
    pending: bool,
}

impl FreshState {
    fn new(initial: bool) -> Self {
        FreshState { pending: initial }
    }

    /// Consume the state for one full network fetch. `forced` is the
    /// one-shot `r` keypress flag (already swapped clear by the caller).
    fn take(&mut self, forced: bool) -> bool {
        let fresh = self.pending || forced;
        self.pending = false;
        fresh
    }
}

fn apply_local_refresh(
    api_events: &[UsageEvent],
    local_events: &mut Vec<UsageEvent>,
    refreshed: Vec<UsageEvent>,
    complete: bool,
) -> Vec<UsageEvent> {
    if complete {
        *local_events = refreshed;
    }
    let mut events = api_events.to_vec();
    events.extend(local_events.iter().cloned());
    events
}

fn provider_color(id: &str) -> Color {
    match id {
        "anthropic" | "claude" => Color::Magenta,
        "openai" | "codex" => Color::Green,
        "deepseek" => Color::Blue,
        "kimi" => Color::Cyan,
        "glm" => Color::Yellow,
        "gemini" => Color::LightBlue,
        "qwen" => Color::LightRed,
        _ => Color::White,
    }
}

fn pct_color(p: f64) -> Color {
    if p < 60.0 {
        Color::Green
    } else if p < 85.0 {
        Color::Yellow
    } else {
        Color::Red
    }
}

pub fn run(
    cfg: Config,
    since_spec: String,
    net_secs: u64,
    local_secs: u64,
    fresh_initial: bool,
) -> Result<()> {
    let (tx, rx) = mpsc::channel::<Snap>();
    let stop = Arc::new(AtomicBool::new(false));
    let force = Arc::new(AtomicBool::new(false));
    let paused = Arc::new(AtomicBool::new(false));

    // ---------------- background fetcher ----------------
    {
        let (stop, force, paused) = (stop.clone(), force.clone(), paused.clone());
        let label = since_spec.clone();
        std::thread::spawn(move || {
            let netd = Duration::from_secs(net_secs.max(15));
            let locald = Duration::from_secs(local_secs.max(1));
            // Fire a full fetch immediately, locals in between.
            let mut last_net = Instant::now() - netd;
            let mut last_local = Instant::now();
            // FR-3.2: a one-shot --fresh bypasses only the initial full
            // network fetch; `r` bypasses exactly one more. Scheduled
            // ticks honor the TTL; local-only ticks never consume it.
            let mut fresh_state = FreshState::new(fresh_initial);
            // Non-local state kept from the last network tick.
            let mut api_events: Vec<UsageEvent> = vec![];
            let mut local_events: Vec<UsageEvent> = vec![];
            let mut billed: Vec<BilledCost> = vec![];
            let mut quotas: Vec<QuotaSnapshot> = vec![];
            let mut balances: Vec<BalanceSnapshot> = vec![];

            loop {
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                let is_paused = paused.load(Ordering::Relaxed);
                let forced = force.swap(false, Ordering::Relaxed);
                let now = Utc::now();
                let since = match crate::parse_since(&label, now) {
                    Ok(s) => s,
                    Err(_) => now - CDuration::days(30),
                };

                if !is_paused && (forced || last_net.elapsed() >= netd) {
                    let fresh = fresh_state.take(forced);
                    let ctx = crate::providers::FetchContext::from_config(&cfg, fresh);
                    let g = crate::gather(&cfg, &ctx, since, now, None, true, true, true);
                    api_events = g
                        .events
                        .iter()
                        .filter(|e| e.source == SourceKind::Api)
                        .cloned()
                        .collect();
                    local_events = g
                        .events
                        .iter()
                        .filter(|e| e.source == SourceKind::LocalLogs)
                        .cloned()
                        .collect();
                    billed = g.billed.clone();
                    quotas = g.quotas.clone();
                    balances = g.balances.clone();
                    last_net = Instant::now();
                    last_local = Instant::now();
                    let snap = Snap {
                        d: Dashboard {
                            events: g.events,
                            quotas: quotas.clone(),
                            balances: balances.clone(),
                            billed: billed.clone(),
                            window_label: label.clone(),
                        },
                        at: now,
                        net: true,
                    };
                    if tx.send(snap).is_err() {
                        return;
                    }
                } else if !is_paused && last_local.elapsed() >= locald {
                    // Local-only: call each file-backed usage source directly.
                    // `gather`'s provider filter is report semantics and also
                    // filters Claude transcript attribution, so it cannot model
                    // which independent local streams this partial tick owns.
                    // No network happens here, so the freshness bypass is
                    // neither consumed nor required (FR-3.2).
                    let ctx = crate::providers::FetchContext::from_config(&cfg, false);
                    let mut refreshed = vec![];
                    let mut complete = true;
                    match crate::local::claude_code::collect(&cfg, since, now) {
                        Ok(c) => refreshed.extend(c.events),
                        Err(_) => complete = false,
                    }
                    for provider in [
                        &crate::providers::codex::Codex as &dyn Provider,
                        &crate::providers::gemini::Gemini,
                        &crate::providers::qwen::Qwen,
                    ] {
                        if provider.configured(&cfg) {
                            match provider.usage(&cfg, &ctx, since, now) {
                                Ok(f) => refreshed.extend(f.events),
                                Err(_) => complete = false,
                            }
                        }
                    }
                    let events =
                        apply_local_refresh(&api_events, &mut local_events, refreshed, complete);
                    last_local = Instant::now();
                    let snap = Snap {
                        d: Dashboard {
                            events,
                            quotas: quotas.clone(),
                            balances: balances.clone(),
                            billed: billed.clone(),
                            window_label: label.clone(),
                        },
                        at: now,
                        net: false,
                    };
                    if tx.send(snap).is_err() {
                        return;
                    }
                }
                std::thread::sleep(Duration::from_millis(200));
            }
        });
    }

    // ---------------- terminal ----------------
    enable_raw_mode().context("tui/watch needs an interactive terminal")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut period = Period::Day;
    let mut d = Dashboard {
        window_label: since_spec.clone(),
        ..Default::default()
    };
    let mut updated: Option<DateTime<Utc>> = None;
    let mut net_updated: Option<DateTime<Utc>> = None;

    let res = loop {
        while let Ok(s) = rx.try_recv() {
            d = s.d;
            updated = Some(s.at);
            if s.net {
                net_updated = Some(s.at);
            }
        }
        let is_paused = paused.load(Ordering::Relaxed);
        if let Err(e) = terminal.draw(|f| draw(f, &d, period, updated, net_updated, is_paused)) {
            break Err(e.into());
        }
        // Event errors break the loop (like draw errors) so the cleanup
        // below always runs — a `?` here would return from run() leaving
        // the terminal in raw mode / alternate screen.
        match event::poll(Duration::from_millis(250)) {
            Err(e) => break Err(e.into()),
            Ok(false) => {}
            Ok(true) => match event::read() {
                Err(e) => break Err(e.into()),
                Ok(Event::Key(k)) => match k.code {
                    KeyCode::Char('q') | KeyCode::Esc => break Ok(()),
                    KeyCode::Char('d') => period = Period::Day,
                    KeyCode::Char('w') => period = Period::Week,
                    KeyCode::Char('m') => period = Period::Month,
                    KeyCode::Char('r') => {
                        force.store(true, Ordering::Relaxed);
                    }
                    KeyCode::Char('p') => {
                        let v = paused.load(Ordering::Relaxed);
                        paused.store(!v, Ordering::Relaxed);
                    }
                    _ => {}
                },
                Ok(_) => {}
            },
        }
    };

    stop.store(true, Ordering::Relaxed);
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    res
}
fn draw(
    f: &mut Frame,
    d: &Dashboard,
    period: Period,
    updated: Option<DateTime<Utc>>,
    net_updated: Option<DateTime<Utc>>,
    paused: bool,
) {
    // Keep the five non-quota layout rows below at their declared minimums.
    const NON_QUOTA_MIN_H: u16 = 3 + 4 + 9 + 6 + 2;
    let quota_rows = u16::try_from(d.quotas.len()).unwrap_or(u16::MAX);
    let requested_quota_h = quota_rows.saturating_add(3).max(4); // header + borders
    let available_quota_h = f.size().height.saturating_sub(NON_QUOTA_MIN_H).max(4);
    let gauge_h = requested_quota_h.min(available_quota_h);
    let visible_quota_rows = gauge_h.saturating_sub(3) as usize;
    let chunks = Layout::vertical([
        Constraint::Length(3),       // header
        Constraint::Length(4),       // activity sparkline (24h)
        Constraint::Length(9),       // bar chart per period
        Constraint::Min(6),          // by-model table
        Constraint::Length(gauge_h), // quota gauges
        Constraint::Length(2),       // balances + footer
    ])
    .split(f.size());

    // --- header: grand totals + freshness ---
    let mut g = report::Totals::default();
    for e in &d.events {
        g.requests += e.requests;
        g.input_tokens += e.input_tokens;
        g.output_tokens += e.output_tokens;
        g.cache_read_tokens += e.cache_read_tokens;
        g.cache_write_tokens += e.cache_write_tokens;
        g.tool_calls += e.tool_calls;
        if let Some(c) = e.cost_usd {
            g.est_cost_usd += c;
            g.has_cost = true;
        }
    }
    let billed_total: f64 = d.billed.iter().map(|b| b.amount_usd).sum();
    let header = Line::from(vec![
        Span::styled(
            format!(" {} ", d.window_label),
            Style::new().bold().fg(Color::Cyan),
        ),
        Span::raw(format!(
            "• req {} • {} • ",
            report::fmt_int(g.requests),
            report::totals_summary(&g),
        )),
        Span::styled(
            format!("est ${:.2}", g.est_cost_usd),
            Style::new().fg(Color::Green),
        ),
        Span::raw(" • "),
        Span::styled(
            format!("billed ${billed_total:.2}"),
            Style::new().fg(Color::Green).bold(),
        ),
    ]);
    let title = Line::from(vec![
        Span::styled(
            " llmu live ",
            Style::new().bold().fg(Color::Black).bg(Color::Cyan),
        ),
        Span::raw(" "),
        if paused {
            Span::styled("PAUSED", Style::new().bold().fg(Color::Yellow))
        } else {
            Span::styled(
                format!(
                    "local {}  net {}",
                    updated
                        .map(|u| u.format("%H:%M:%S").to_string())
                        .unwrap_or_else(|| "…".into()),
                    net_updated
                        .map(|u| u.format("%H:%M:%S").to_string())
                        .unwrap_or_else(|| "…".into()),
                ),
                Style::new().dim(),
            )
        },
    ]);
    f.render_widget(
        Paragraph::new(header).block(Block::bordered().title(title)),
        chunks[0],
    );

    // --- sparkline: total tokens per hour, last 24h ---
    let now = Utc::now();
    let mut hours: BTreeMap<DateTime<Utc>, u64> = BTreeMap::new();
    for i in 0..24 {
        let h = (now - CDuration::hours(i))
            .duration_trunc(CDuration::hours(1))
            .unwrap_or(now);
        hours.insert(h, 0);
    }
    for e in &d.events {
        if let Some(v) = hours.get_mut(
            &e.start
                .duration_trunc(CDuration::hours(1))
                .unwrap_or(e.start),
        ) {
            *v += e.total_tokens();
        }
    }
    let spark: Vec<u64> = hours.values().copied().collect();
    f.render_widget(
        Sparkline::default()
            .block(Block::bordered().title(" activity — tokens/hour, last 24h "))
            .style(Style::new().fg(Color::Cyan))
            .data(&spark),
        chunks[1],
    );

    // --- bar chart: total tokens per period bucket ---
    let mut buckets: BTreeMap<String, u64> = BTreeMap::new();
    for e in &d.events {
        *buckets.entry(period.key(e.start)).or_default() += e.total_tokens();
    }
    let labels: Vec<(String, u64)> = buckets
        .into_iter()
        .rev()
        .take(14)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .map(|(k, v)| (k.get(5..).unwrap_or(&k).to_string(), v / 1000))
        .collect();
    let data: Vec<(&str, u64)> = labels.iter().map(|(k, v)| (k.as_str(), *v)).collect();
    f.render_widget(
        BarChart::default()
            .block(
                Block::bordered().title(format!(" ktok / {} — d/w/m to switch ", period.label())),
            )
            .bar_width(7)
            .bar_gap(1)
            .bar_style(Style::new().fg(Color::Cyan))
            .value_style(Style::new().fg(Color::Black).bg(Color::Cyan))
            .data(&data),
        chunks[2],
    );

    // --- table: provider + model totals, colored per provider ---
    let rows_agg = report::aggregate(
        &d.events,
        Period::Month,
        &[Group::Provider, Group::Model, Group::Source],
    );
    let mut merged: BTreeMap<(String, String, String), report::Totals> = BTreeMap::new();
    for r in rows_agg {
        let key = (r.keys[1].clone(), r.keys[2].clone(), r.keys[3].clone());
        let t = merged.entry(key).or_default();
        t.requests += r.totals.requests;
        t.input_tokens += r.totals.input_tokens;
        t.output_tokens += r.totals.output_tokens;
        t.cache_read_tokens += r.totals.cache_read_tokens;
        t.cache_write_tokens += r.totals.cache_write_tokens;
        t.tool_calls += r.totals.tool_calls;
        t.est_cost_usd += r.totals.est_cost_usd;
        t.has_cost |= r.totals.has_cost;
    }
    let mut sorted: Vec<_> = merged.into_iter().collect();
    sorted.sort_by_key(|(_, u)| std::cmp::Reverse(u.total_tokens()));
    let table_rows: Vec<ratatui::widgets::Row> = sorted
        .iter()
        .map(|((prov, model, src), t)| {
            ratatui::widgets::Row::new(vec![
                prov.clone(),
                model.clone(),
                src.clone(),
                report::fmt_int(t.requests),
                report::fmt_compact(t.total_tokens()),
                report::fmt_cost(t),
            ])
            .style(Style::new().fg(provider_color(prov)))
        })
        .collect();
    let widths = [
        Constraint::Length(10),
        Constraint::Min(24),
        Constraint::Length(6),
        Constraint::Length(9),
        Constraint::Length(14),
        Constraint::Length(10),
    ];
    f.render_widget(
        Table::new(table_rows, widths)
            .header(
                ratatui::widgets::Row::new(vec![
                    "Provider", "Model", "Src", "Req", "Tokens", "Est$",
                ])
                .style(Style::new().bold().fg(Color::Cyan)),
            )
            .block(Block::bordered().title({
                use std::collections::BTreeSet;
                let feeds: BTreeSet<String> = d
                    .events
                    .iter()
                    .map(|e| {
                        format!(
                            "{}·{}",
                            e.provider,
                            match e.source {
                                SourceKind::Api => "api",
                                SourceKind::LocalLogs => "local",
                            }
                        )
                    })
                    .collect();
                if feeds.is_empty() {
                    " by model ".to_string()
                } else {
                    format!(
                        " by model — counting: {} ",
                        feeds.into_iter().collect::<Vec<_>>().join(", ")
                    )
                }
            })),
        chunks[3],
    );

    // --- quotas: data table with a separate progress-bar column ---
    let hidden_quota_rows = d.quotas.len().saturating_sub(visible_quota_rows);
    let quota_title = if hidden_quota_rows == 0 {
        " subscription quotas ".to_string()
    } else {
        format!(" subscription quotas ({hidden_quota_rows} more) ")
    };
    let qblock = Block::bordered().title(quota_title);
    let inner = qblock.inner(chunks[4]);
    f.render_widget(qblock, chunks[4]);
    if d.quotas.is_empty() {
        f.render_widget(
            Paragraph::new("no quota sources detected").style(Style::new().dim()),
            inner,
        );
    } else {
        const BAR_W: usize = 20;
        let qrows: Vec<ratatui::widgets::Row> = d
            .quotas
            .iter()
            .take(visible_quota_rows)
            .map(|q| {
                let pcol = provider_color(&q.provider);
                if q.limit > 0.0 {
                    let pct = q.pct().clamp(0.0, 100.0);
                    let filled = ((pct / 100.0) * BAR_W as f64).round() as usize;
                    let bar = Line::from(vec![
                        Span::styled(
                            "█".repeat(filled.min(BAR_W)),
                            Style::new().fg(pct_color(pct)),
                        ),
                        Span::styled(
                            "─".repeat(BAR_W - filled.min(BAR_W)),
                            Style::new().fg(Color::DarkGray),
                        ),
                    ]);
                    ratatui::widgets::Row::new(vec![
                        Cell::from(q.provider.clone()).style(Style::new().fg(pcol)),
                        Cell::from(q.plan.clone()),
                        Cell::from(q.window.clone()),
                        Cell::from(if q.unit == "%" {
                            "—".to_string()
                        } else {
                            format!(
                                "{}/{} {}",
                                report::fmt_int(q.used.max(0.0) as u64),
                                report::fmt_int(q.limit as u64),
                                q.unit
                            )
                        }),
                        Cell::from(format!("{pct:>5.1}%"))
                            .style(Style::new().fg(pct_color(pct)).bold()),
                        Cell::from(bar),
                        Cell::from(
                            q.resets_at
                                .map(|r| r.format("%m-%d %H:%M").to_string())
                                .unwrap_or_else(|| "—".into()),
                        )
                        .style(Style::new().dim()),
                    ])
                } else {
                    ratatui::widgets::Row::new(vec![
                        Cell::from(q.provider.clone()).style(Style::new().fg(pcol)),
                        Cell::from(q.plan.clone()),
                        Cell::from(q.window.clone()),
                        Cell::from(format!("{} {}", report::fmt_int(q.used as u64), q.unit)),
                        Cell::from("—"),
                        Cell::from(Span::styled("no known limit", Style::new().dim())),
                        Cell::from("—").style(Style::new().dim()),
                    ])
                }
            })
            .collect();
        let qwidths = [
            Constraint::Length(9),            // provider
            Constraint::Min(18),              // plan
            Constraint::Length(4),            // window
            Constraint::Length(24),           // used/limit
            Constraint::Length(6),            // %
            Constraint::Length(BAR_W as u16), // bar
            Constraint::Length(11),           // resets
        ];
        f.render_widget(
            Table::new(qrows, qwidths).column_spacing(2).header(
                ratatui::widgets::Row::new(vec![
                    "Provider", "Plan", "Win", "Used", "%", "Progress", "Resets",
                ])
                .style(Style::new().bold().fg(Color::Cyan)),
            ),
            inner,
        );
    }

    // --- balances + footer ---
    let bal: Vec<Span> = d
        .balances
        .iter()
        .flat_map(|b| {
            vec![
                Span::styled(
                    format!("{}: ", b.provider),
                    Style::new().fg(provider_color(&b.provider)),
                ),
                Span::styled(
                    format!("{:.2} {}", b.total, b.currency),
                    Style::new().fg(Color::Green),
                ),
                Span::raw("   "),
            ]
        })
        .collect();
    let footer = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).split(chunks[5]);
    f.render_widget(Paragraph::new(Line::from(bal)), footer[0]);
    f.render_widget(
        Paragraph::new(" q quit • d/w/m period • r refresh now • p pause")
            .style(Style::new().dim()),
        footer[1],
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quota(provider: &str, window: &str) -> QuotaSnapshot {
        QuotaSnapshot {
            provider: provider.into(),
            plan: "test plan".into(),
            window: window.into(),
            used: 1.0,
            limit: 10.0,
            unit: "credits".into(),
            resets_at: None,
        }
    }

    fn usage(provider: &str, source: SourceKind) -> UsageEvent {
        UsageEvent {
            provider: provider.into(),
            source,
            model: format!("{provider}-model"),
            start: Utc::now(),
            requests: 1,
            input_tokens: 10,
            output_tokens: 20,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            tool_calls: 0,
            cost_usd: None,
            cost_is_estimate: false,
        }
    }

    #[test]
    fn incomplete_local_refresh_preserves_the_previous_local_snapshot() {
        let api = vec![usage("openai", SourceKind::Api)];
        let mut local = vec![
            usage("anthropic", SourceKind::LocalLogs),
            usage("qwen", SourceKind::LocalLogs),
        ];

        let next = apply_local_refresh(
            &api,
            &mut local,
            vec![usage("codex", SourceKind::LocalLogs)],
            false,
        );
        let providers: Vec<&str> = next.iter().map(|e| e.provider.as_str()).collect();

        assert_eq!(providers, vec!["openai", "anthropic", "qwen"]);
    }

    #[test]
    fn complete_local_refresh_replaces_local_rows_and_preserves_api_rows() {
        let api = vec![usage("openai", SourceKind::Api)];
        let mut local = vec![usage("anthropic", SourceKind::LocalLogs)];

        let next = apply_local_refresh(
            &api,
            &mut local,
            vec![
                usage("anthropic", SourceKind::LocalLogs),
                usage("codex", SourceKind::LocalLogs),
                usage("gemini", SourceKind::LocalLogs),
                usage("qwen", SourceKind::LocalLogs),
            ],
            true,
        );
        let providers: Vec<&str> = next.iter().map(|e| e.provider.as_str()).collect();

        assert_eq!(
            providers,
            vec!["openai", "anthropic", "codex", "gemini", "qwen"]
        );
    }

    #[test]
    fn quota_panel_renders_rows_beyond_the_first_six() {
        let d = Dashboard {
            quotas: vec![
                quota("claude", "5h"),
                quota("claude", "7d"),
                quota("codex", "5h"),
                quota("codex", "7d"),
                quota("kimi", "5h"),
                quota("kimi", "7d"),
                quota("glm", "5h"),
                quota("glm", "1w"),
            ],
            ..Default::default()
        };
        let backend = ratatui::backend::TestBackend::new(160, 40);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|f| draw(f, &d, Period::Day, None, None, false))
            .unwrap();

        let rendered = terminal.backend().to_string();
        assert_eq!(
            rendered.matches("glm").count(),
            2,
            "the quota panel silently dropped GLM rows:\n{rendered}"
        );
    }

    #[test]
    fn compact_quota_panel_reports_overflow_and_keeps_the_footer() {
        let d = Dashboard {
            quotas: (0..12).map(|n| quota("glm", &format!("{n}h"))).collect(),
            ..Default::default()
        };
        let backend = ratatui::backend::TestBackend::new(160, 33);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|f| draw(f, &d, Period::Day, None, None, false))
            .unwrap();

        let rendered = terminal.backend().to_string();
        assert!(
            rendered.contains("subscription quotas (6 more)"),
            "quota overflow must be explicit:\n{rendered}"
        );
        assert!(
            rendered.contains("q quit"),
            "quota rows must not crowd the footer off-screen:\n{rendered}"
        );
    }

    /// Task 7 (RED): a one-shot `--fresh` bypasses only the initial full
    /// network fetch; later scheduled ticks honor the TTL (FR-3.2).
    #[test]
    fn fresh_bypasses_only_the_initial_full_fetch() {
        let mut s = FreshState::new(true);
        assert!(s.take(false), "the initial network tick bypasses");
        assert!(!s.take(false), "scheduled ticks honor the TTL");
        assert!(!s.take(false));
    }

    /// Task 7 (RED): `r` bypasses exactly the next full network fetch
    /// and then clears (FR-3.2); a later `r` re-arms it.
    #[test]
    fn r_bypasses_exactly_one_fetch_then_clears() {
        let mut s = FreshState::new(false);
        assert!(s.take(true), "the forced fetch bypasses");
        assert!(!s.take(false), "the next scheduled tick honors the TTL");
        assert!(s.take(true), "a later r re-arms the bypass");
        assert!(!s.take(false));
    }

    /// Task 7 (RED): local-only ticks neither consume nor require the
    /// network bypass — the pending initial bypass survives until the
    /// first full network fetch.
    #[test]
    fn local_only_ticks_do_not_consume_the_bypass() {
        let mut s = FreshState::new(true);
        // (local ticks never call take)
        assert!(s.take(false), "the first full fetch still bypasses");
        assert!(!s.take(false));
    }

    /// The full TUI gather and direct local-provider usage path both
    /// receive the typed fetch context; no hidden freshness state exists.
    #[test]
    fn tui_gather_sites_pass_the_fetch_context() {
        let needle = ["crate::", "gat", "her("].concat();
        let src = include_str!("tui.rs");
        let sites: Vec<&str> = src.lines().filter(|l| l.contains(&needle)).collect();
        assert_eq!(sites.len(), 1, "only the network tick uses gather");
        for line in &sites {
            assert!(
                line.contains("&ctx"),
                "every TUI gather must pass the context, got: {line}"
            );
        }
        assert!(
            src.contains("provider.usage(&cfg, &ctx, since, now)"),
            "direct local-provider usage must receive the context"
        );
    }

    /// FR-6: the TUI renders the qwen provider in light red.
    #[test]
    fn provider_color_maps_qwen_to_light_red() {
        assert_eq!(provider_color("qwen"), Color::LightRed);
    }

    /// The header names the cache bucket explicitly so the total
    /// reconciles with fresh in/out, and cache-scale numbers render
    /// compact instead of as 13-digit integers.
    #[test]
    fn header_labels_cached_tokens_separately_from_fresh_in_out() {
        let d = Dashboard {
            events: vec![UsageEvent {
                provider: "anthropic".into(),
                source: SourceKind::LocalLogs,
                model: "claude-opus-5".into(),
                start: Utc::now(),
                requests: 14_754,
                input_tokens: 310_134,
                output_tokens: 11_088_638,
                cache_read_tokens: 4_613_902_660,
                cache_write_tokens: 0,
                tool_calls: 0,
                cost_usd: None,
                cost_is_estimate: false,
            }],
            window_label: "30d".into(),
            ..Default::default()
        };
        let backend = ratatui::backend::TestBackend::new(160, 40);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|f| draw(f, &d, Period::Day, None, None, false))
            .unwrap();

        let rendered = terminal.backend().to_string();
        assert!(
            rendered.contains("tok 4.63B (in 310k / out 11.1M / cache 4.61B)"),
            "header must label cache tokens so the total reconciles:\n{rendered}"
        );
    }
}
