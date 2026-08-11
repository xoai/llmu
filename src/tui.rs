//! Live dashboard (`llmu tui` / `llmu watch`).
//!
//! Two refresh cadences run on a background worker thread so the UI
//! never blocks:
//!   - LOCAL tick (default 3s): re-parses Claude Code / Codex session
//!     logs only — mtime-filtered, milliseconds of work, safe to poll.
//!   - NETWORK tick (default 60s): full fetch including provider usage
//!     APIs, quotas, and balances. Kept slow deliberately — the Claude
//!     oauth/usage endpoint rate-limits aggressively.
//!
//! Keys: q quit • d/w/m period • r force network refresh • p pause.

use crate::config::Config;
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

fn provider_color(id: &str) -> Color {
    match id {
        "anthropic" | "claude" => Color::Magenta,
        "openai" | "codex" => Color::Green,
        "deepseek" => Color::Blue,
        "kimi" => Color::Cyan,
        "glm" => Color::Yellow,
        "gemini" => Color::LightBlue,
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

pub fn run(cfg: Config, since_spec: String, net_secs: u64, local_secs: u64) -> Result<()> {
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
            // Non-local state kept from the last network tick.
            let mut api_events: Vec<UsageEvent> = vec![];
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
                    let g = crate::gather(&cfg, since, now, None, true, true, true);
                    api_events = g
                        .events
                        .iter()
                        .filter(|e| e.source == SourceKind::Api)
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
                    // Local-only: codex provider (file-based); the Claude
                    // Code transcript collector always runs inside gather.
                    let filt = ["codex".to_string()];
                    let l = crate::gather(&cfg, since, now, Some(&filt), true, false, false);
                    let mut events = api_events.clone();
                    events.extend(
                        l.events
                            .into_iter()
                            .filter(|e| e.source == SourceKind::LocalLogs),
                    );
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
    let gauge_h = (d.quotas.len().min(6) as u16 + 3).max(4); // header + borders
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
            "• req {} • tok {} (in {} / out {}) • ",
            report::fmt_int(g.requests),
            report::fmt_int(g.total_tokens()),
            report::fmt_int(g.input_tokens),
            report::fmt_int(g.output_tokens),
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
                report::fmt_int(t.total_tokens()),
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
    let qblock = Block::bordered().title(" subscription quotas ");
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
            .take(6)
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
