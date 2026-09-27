//! Live dashboard (`llmu tui` / `llmu watch`).
//!
//! Three refresh cadences run on a background worker thread so the UI
//! never blocks:
//!   - LOCAL tick (default 3s): re-parses Claude Code, Codex, Gemini,
//!     Qwen, and OpenCode local usage records — no network requests.
//!   - USAGE tick (default 60s): provider usage APIs and billed cost.
//!   - QUOTA tick (default 300s, backing off): quotas and balances.
//!
//! Quotas ride their own slow cadence because Anthropic's
//! `oauth/usage` endpoint throttles at roughly a poll a minute and
//! answers **429**, at which point `gather` quietly substitutes
//! last-known-good meters. Folding quotas into the 60s usage tick
//! therefore parked the dashboard inside the throttle window: the
//! Anthropic meters froze on a stale percentage for as long as the
//! dashboard stayed open, while the header kept advertising a fresh
//! network time — the "it only shows the real number if I restart it"
//! failure. Two things fix it: poll slowly enough to stay under the
//! limit (with [`QuotaBackoff`] widening further whenever a tick still
//! comes back throttled), and never freeze silently — the TUI has no
//! stderr, so [`quota_title`] says on the panel itself which meters are
//! not live and when the next attempt lands.
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
    pub quota: QuotaStatus,
}

/// Freshness of the quota panel.
///
/// `gather` reports a failed quota fetch through `Gathered::quota_stale`
/// and substitutes last-known-good meters, which are indistinguishable
/// from live ones once rendered. The CLI prints the reason to stderr; a
/// full-screen dashboard has nowhere to print, so it carries the state
/// here and renders it into the panel title instead.
#[derive(Clone, Default)]
pub struct QuotaStatus {
    /// Providers whose meters were not observed live on the last quota
    /// tick — absent, or served from the last-known-good cache.
    pub stale: Vec<String>,
    /// When every meter was last observed live.
    pub live_at: Option<DateTime<Utc>>,
    /// When the next quota tick is due, backoff included.
    pub next_at: Option<DateTime<Utc>>,
}

struct Snap {
    d: Dashboard,
    at: DateTime<Utc>,
    net: bool, // was this a network refresh (usage and/or quota)?
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

/// Retry pacing for the quota/balance cadence.
///
/// A throttled quota tick that retries on the same cadence just stays
/// inside the throttle window, so the meters stay frozen for the life of
/// the dashboard. Each not-live tick doubles the interval up to [`CAP`];
/// the first live tick snaps straight back to the configured base, so a
/// single transient 429 costs one slow cycle, not the session.
///
/// [`CAP`]: QuotaBackoff::CAP
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct QuotaBackoff {
    base: Duration,
    current: Duration,
}

impl QuotaBackoff {
    /// Hard floor on the cadence whatever `--quota-refresh` says: below
    /// this llmu is the reason the endpoint throttles.
    const FLOOR: u64 = 30;
    /// A 5h/7d meter reported in whole percent is still useful half an
    /// hour old; past that the panel would just be wrong.
    const CAP: Duration = Duration::from_secs(1800);

    fn new(secs: u64) -> Self {
        let base = Duration::from_secs(secs.max(Self::FLOOR));
        QuotaBackoff {
            base,
            current: base,
        }
    }

    fn interval(&self) -> Duration {
        self.current
    }

    /// Every meter came back live: drop the penalty entirely.
    fn on_live(&mut self) {
        self.current = self.base;
    }

    /// At least one provider's meters were not live. `max(base)` keeps a
    /// base above `CAP` (an explicitly slow `--quota-refresh`) from being
    /// sped up by a failure; the checked doubling keeps an absurd
    /// `--quota-refresh` from panicking on overflow instead of just
    /// being slow.
    fn on_stale(&mut self) {
        self.current = self
            .current
            .checked_mul(2)
            .unwrap_or(Self::CAP)
            .min(Self::CAP)
            .max(self.base);
    }
}

/// Quota panel title, including the not-live banner.
///
/// Pure so the one place a throttled quota fetch can announce itself is
/// testable without a terminal.
fn quota_title(status: &QuotaStatus, hidden: usize, now: DateTime<Utc>) -> String {
    let mut t = String::from(" subscription quotas");
    if hidden > 0 {
        t.push_str(&format!(" ({hidden} more)"));
    }
    if !status.stale.is_empty() {
        t.push_str(&format!(" — {} not live", status.stale.join(", ")));
        if let Some(at) = status.live_at {
            t.push_str(&format!(", last {}", at.format("%H:%M:%S")));
        }
        if let Some(next) = status.next_at {
            t.push_str(&format!(", retry {}s", (next - now).num_seconds().max(0)));
        }
        t.push_str(" — r to retry now");
    }
    t.push(' ');
    t
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

/// Task 6 (FR-27/AC-5): a database-level OpenCode failure downgrades the
/// whole partial local refresh so `apply_local_refresh` preserves the
/// previous complete snapshot. Only the two fixed database notes count —
/// bounded malformed/unsupported skip notes are a completed query whose
/// valid events stay. OpenCode's note constants are private to the
/// collector, so the exact literals are pinned here.
fn opencode_db_failed(notes: &[String]) -> bool {
    const BUSY_OR_UNREADABLE: &str = "opencode: local usage database is busy or unreadable";
    const INCOMPATIBLE: &str = "opencode: local usage database schema is unsupported";
    notes
        .iter()
        .any(|n| n == BUSY_OR_UNREADABLE || n == INCOMPATIBLE)
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
    quota_secs: u64,
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
            let mut backoff = QuotaBackoff::new(quota_secs);
            // Fire both network fetches immediately, locals in between.
            let mut last_net = Instant::now() - netd;
            let mut last_quota = Instant::now() - backoff.interval();
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
            let mut quota_status = QuotaStatus::default();

            loop {
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                let is_paused = paused.load(Ordering::Relaxed);
                // Consuming the flag while paused would silently discard
                // the keypress; leave it set so unpausing honors it.
                let forced = !is_paused && force.swap(false, Ordering::Relaxed);
                let now = Utc::now();
                let since = match crate::parse_since(&label, now) {
                    Ok(s) => s,
                    Err(_) => now - CDuration::days(30),
                };

                let usage_due = forced || last_net.elapsed() >= netd;
                let quota_due = forced || last_quota.elapsed() >= backoff.interval();
                if !is_paused && (usage_due || quota_due) {
                    // One bypass per network iteration, whichever tick
                    // (or both) is due — `--fresh`/`r` semantics are per
                    // fetch round, not per endpoint group (FR-3.2).
                    let fresh = fresh_state.take(forced);
                    let ctx = crate::providers::FetchContext::from_config(&cfg, fresh);
                    if quota_due {
                        // Quotas and balances only: this is the cadence
                        // that must stay clear of oauth/usage throttling.
                        let g = crate::gather(&cfg, &ctx, since, now, None, false, true, true);
                        quotas = g.quotas;
                        balances = g.balances;
                        if g.quota_stale.is_empty() {
                            backoff.on_live();
                            quota_status.live_at = Some(now);
                        } else {
                            backoff.on_stale();
                        }
                        quota_status.stale = g.quota_stale;
                        // An out-of-range cadence yields no countdown
                        // rather than a panicking date arithmetic.
                        quota_status.next_at = chrono::Duration::from_std(backoff.interval())
                            .ok()
                            .and_then(|d| now.checked_add_signed(d));
                        last_quota = Instant::now();
                    }
                    if usage_due {
                        let g = crate::gather(&cfg, &ctx, since, now, None, true, false, false);
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
                        billed = g.billed;
                        last_net = Instant::now();
                        last_local = Instant::now();
                    }
                    let mut events = api_events.clone();
                    events.extend(local_events.iter().cloned());
                    let snap = Snap {
                        d: Dashboard {
                            events,
                            quotas: quotas.clone(),
                            balances: balances.clone(),
                            billed: billed.clone(),
                            window_label: label.clone(),
                            quota: quota_status.clone(),
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
                    // OpenCode is another file-backed local stream with
                    // its own read-only database; collect it directly,
                    // never through gather. A database-level failure (the
                    // two fixed notes) downgrades the whole tick so the
                    // previous snapshot survives; bounded skip notes are
                    // a completed query and keep their valid events.
                    let opencode = crate::local::opencode::collect(&cfg, since, now);
                    if opencode_db_failed(&opencode.notes) {
                        complete = false;
                    } else {
                        refreshed.extend(opencode.events);
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
                            quota: quota_status.clone(),
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
        g.add_event(e);
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
            *v = v.saturating_add(e.total_tokens());
        }
    }
    // Scale to ktok before handing values to the widget: a saturated
    // u64::MAX hour bucket would overflow the widget's internal
    // value*height*8 scaling in debug builds (FR-24).
    let spark: Vec<u64> = hours.values().map(|v| v / 1000).collect();
    f.render_widget(
        Sparkline::default()
            .block(Block::bordered().title(" activity — ktok/hour, last 24h "))
            .style(Style::new().fg(Color::Cyan))
            .data(&spark),
        chunks[1],
    );

    // --- bar chart: total tokens per period bucket ---
    let mut buckets: BTreeMap<String, u64> = BTreeMap::new();
    for e in &d.events {
        let entry = buckets.entry(period.key(e.start)).or_default();
        *entry = entry.saturating_add(e.total_tokens());
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
        merged.entry(key).or_default().add_totals(&r.totals);
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
    // `now` is this frame's clock, so the retry countdown ticks down
    // between snapshots instead of only when a new one lands.
    let qtitle = quota_title(&d.quota, hidden_quota_rows, now);
    let qstyle = if d.quota.stale.is_empty() {
        Style::new()
    } else {
        Style::new().fg(Color::Yellow)
    };
    let qblock = Block::bordered().title(Span::styled(qtitle, qstyle));
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

    /// A throttled quota tick must not retry on the same cadence: that
    /// keeps llmu inside the throttle window and freezes the meters for
    /// the life of the dashboard.
    #[test]
    fn backoff_widens_on_throttle_and_snaps_back_on_the_first_live_tick() {
        let mut b = QuotaBackoff::new(300);
        assert_eq!(b.interval(), Duration::from_secs(300));
        b.on_stale();
        assert_eq!(b.interval(), Duration::from_secs(600));
        b.on_stale();
        assert_eq!(b.interval(), Duration::from_secs(1200));
        // One good answer is enough; a transient 429 costs one cycle.
        b.on_live();
        assert_eq!(b.interval(), Duration::from_secs(300));
    }

    #[test]
    fn backoff_is_floored_capped_and_never_speeds_up_a_slow_base() {
        // Below the floor llmu would be the reason for the throttling.
        assert_eq!(
            QuotaBackoff::new(1).interval(),
            Duration::from_secs(QuotaBackoff::FLOOR)
        );

        let mut b = QuotaBackoff::new(1200);
        for _ in 0..10 {
            b.on_stale();
        }
        assert_eq!(b.interval(), QuotaBackoff::CAP, "meters must not rot");

        // A base past the cap is a deliberate choice; failure must not
        // override it into faster polling.
        let mut slow = QuotaBackoff::new(3600);
        slow.on_stale();
        assert_eq!(slow.interval(), Duration::from_secs(3600));

        // An absurd cadence must stay slow, not panic on overflow.
        let mut absurd = QuotaBackoff::new(u64::MAX);
        absurd.on_stale();
        assert_eq!(absurd.interval(), Duration::from_secs(u64::MAX));
    }

    /// The dashboard has no stderr, so the panel title is the only place
    /// a substituted last-known-good meter can announce itself. Silence
    /// here is the whole bug: frozen numbers under a fresh-looking clock.
    #[test]
    fn quota_title_names_stale_providers_the_last_live_time_and_the_retry() {
        let now = Utc::now();
        let status = QuotaStatus {
            stale: vec!["claude".into()],
            live_at: Some(now - CDuration::seconds(240)),
            next_at: Some(now + CDuration::seconds(90)),
        };
        let t = quota_title(&status, 0, now);
        assert!(t.contains("claude not live"), "{t}");
        assert!(
            t.contains(
                &(now - CDuration::seconds(240))
                    .format("%H:%M:%S")
                    .to_string()
            ),
            "the last live time must be visible: {t}"
        );
        assert!(t.contains("retry 90s"), "{t}");
        assert!(t.contains("r to retry now"), "{t}");
    }

    #[test]
    fn quota_title_is_plain_when_every_meter_is_live() {
        let now = Utc::now();
        let live = QuotaStatus {
            stale: vec![],
            live_at: Some(now),
            next_at: Some(now + CDuration::seconds(300)),
        };
        assert_eq!(quota_title(&live, 0, now), " subscription quotas ");
        assert_eq!(
            quota_title(&live, 3, now),
            " subscription quotas (3 more) ",
            "the hidden-row count survives"
        );
    }

    /// An overdue retry renders as 0s, never as a negative countdown.
    #[test]
    fn quota_title_retry_countdown_never_goes_negative() {
        let now = Utc::now();
        let status = QuotaStatus {
            stale: vec!["claude".into(), "glm".into()],
            live_at: None,
            next_at: Some(now - CDuration::seconds(30)),
        };
        let t = quota_title(&status, 0, now);
        assert!(t.contains("claude, glm not live"), "{t}");
        assert!(t.contains("retry 0s"), "{t}");
        assert!(!t.contains("-"), "no negative countdown: {t}");
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

    /// Task 6 (FR-27): the predicate recognizes exactly the two fixed
    /// database-level OpenCode failure notes — never the bounded
    /// malformed/unsupported skip notes, which are a completed query.
    #[test]
    fn opencode_db_failure_predicate_matches_only_the_two_fixed_notes() {
        assert!(
            opencode_db_failed(&["opencode: local usage database is busy or unreadable".into()]),
            "busy/unreadable must downgrade the partial refresh"
        );
        assert!(
            opencode_db_failed(&["opencode: local usage database schema is unsupported".into()]),
            "unsupported schema must downgrade the partial refresh"
        );
        assert!(
            !opencode_db_failed(&["opencode: skipped 3 malformed local usage record(s)".into()]),
            "bounded malformed skip notes are a completed query"
        );
        assert!(
            !opencode_db_failed(&[
                ("opencode: skipped 2 local usage record(s) from unsupported provider(s)".into())
            ]),
            "bounded unsupported-provider skip notes are a completed query"
        );
        assert!(
            !opencode_db_failed(&[]),
            "a missing database (silent, empty) is a successful refresh"
        );
    }

    /// Task 6 (FR-27/FR-28): a mixed snapshot holding canonical
    /// qwen/glm/deepseek/kimi/openai local rows plus unrelated streams
    /// must survive an incomplete partial refresh untouched.
    #[test]
    fn opencode_canonical_rows_survive_incomplete_partial_refresh() {
        let api = vec![usage("openai", SourceKind::Api)];
        let mut local = vec![
            usage("anthropic", SourceKind::LocalLogs),
            usage("codex", SourceKind::LocalLogs),
            usage("gemini", SourceKind::LocalLogs),
            usage("qwen", SourceKind::LocalLogs),
            usage("glm", SourceKind::LocalLogs),
            usage("deepseek", SourceKind::LocalLogs),
            usage("kimi", SourceKind::LocalLogs),
            usage("openai", SourceKind::LocalLogs),
        ];

        let next = apply_local_refresh(&api, &mut local, vec![], false);
        let providers: Vec<&str> = next.iter().map(|e| e.provider.as_str()).collect();

        assert_eq!(
            providers,
            vec![
                "openai",
                "anthropic",
                "codex",
                "gemini",
                "qwen",
                "glm",
                "deepseek",
                "kimi",
                "openai"
            ],
            "a failed OpenCode refresh must preserve the entire prior local snapshot"
        );
    }

    /// Task 6 (FR-27/FR-28): a complete partial refresh replaces local
    /// rows with the combined Claude/Codex/Gemini/Qwen/OpenCode streams
    /// and always preserves API rows.
    #[test]
    fn complete_refresh_replaces_with_combined_streams_including_opencode_rows() {
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
                usage("glm", SourceKind::LocalLogs),
                usage("deepseek", SourceKind::LocalLogs),
                usage("kimi", SourceKind::LocalLogs),
                usage("openai", SourceKind::LocalLogs),
            ],
            true,
        );
        let providers: Vec<&str> = next.iter().map(|e| e.provider.as_str()).collect();

        assert_eq!(
            providers,
            vec![
                "openai",
                "anthropic",
                "codex",
                "gemini",
                "qwen",
                "glm",
                "deepseek",
                "kimi",
                "openai"
            ]
        );
    }

    /// Task 6 (FR-27): OpenCode rows render with the canonical provider,
    /// their model, and `local` provenance in the by-model feed title;
    /// the footer and layout stay visible.
    #[test]
    fn opencode_canonical_rows_render_local_provenance_and_keep_the_footer() {
        let mk = |provider: &str| UsageEvent {
            provider: provider.into(),
            source: SourceKind::LocalLogs,
            model: format!("{provider}-code"),
            start: DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
            requests: 1,
            input_tokens: 10,
            output_tokens: 20,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            tool_calls: 0,
            cost_usd: None,
            cost_is_estimate: true,
        };
        let d = Dashboard {
            events: vec![
                mk("qwen"),
                mk("glm"),
                mk("deepseek"),
                mk("kimi"),
                mk("openai"),
            ],
            window_label: "30d".into(),
            ..Default::default()
        };
        let backend = ratatui::backend::TestBackend::new(160, 40);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|f| draw(f, &d, Period::Day, None, None, false))
            .unwrap();

        let rendered = terminal.backend().to_string();
        for provider in ["qwen", "glm", "deepseek", "kimi", "openai"] {
            assert!(
                rendered.contains(&format!("{provider}·local")),
                "the by-model feed title must show {provider} with local provenance:\n{rendered}"
            );
            assert!(
                rendered.contains(&format!("{provider}-code")),
                "the by-model table must render the canonical {provider} model row:\n{rendered}"
            );
        }
        assert!(
            rendered.contains("q quit"),
            "OpenCode rows must not crowd the footer off-screen:\n{rendered}"
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
        assert_eq!(
            sites.len(),
            2,
            "exactly two network ticks use gather: usage and quota"
        );
        for line in &sites {
            assert!(
                line.contains("&ctx"),
                "every TUI gather must pass the context, got: {line}"
            );
        }
        // The split is the fix, not an accident: quotas must never ride
        // the fast usage cadence onto a throttling endpoint again.
        assert!(
            sites.iter().any(|l| l.contains("None, true, false, false")),
            "one site fetches usage without quotas or balances: {sites:?}"
        );
        assert!(
            sites.iter().any(|l| l.contains("None, false, true, true")),
            "one site fetches quotas and balances without usage: {sites:?}"
        );
        assert!(
            src.contains("provider.usage(&cfg, &ctx, since, now)"),
            "direct local-provider usage must receive the context"
        );
    }

    /// Task 5 (FR-24): the header aggregation, hourly sparkline, period
    /// buckets, and merged by-model totals all saturate — a dashboard
    /// holding a maximum-value OpenCode event plus a positive event must
    /// render every path without panic or wrap, and the by-model feed
    /// title must show the canonical provider with `local` provenance.
    #[test]
    fn saturated_local_events_render_header_chart_and_model_paths_without_panic() {
        let mk = |requests: u64, input: u64, output: u64, cr: u64, cw: u64| UsageEvent {
            provider: "qwen".into(),
            source: SourceKind::LocalLogs,
            model: "qwen-max".into(),
            start: Utc::now(),
            requests,
            input_tokens: input,
            output_tokens: output,
            cache_read_tokens: cr,
            cache_write_tokens: cw,
            tool_calls: 0,
            cost_usd: None,
            cost_is_estimate: true,
        };
        let d = Dashboard {
            events: vec![
                mk(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u64::MAX),
                mk(1, 1, 2, 1, 1),
            ],
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
            rendered.contains("18,446,744,073,709,551,615"),
            "the header requests grand total must saturate without panic:\n{rendered}"
        );
        assert!(
            rendered.contains("qwen·local"),
            "the by-model feed title must show the canonical provider with local provenance:\n{rendered}"
        );
        assert!(
            rendered.contains("by model"),
            "the by-model panel must render:\n{rendered}"
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
