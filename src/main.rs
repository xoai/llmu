mod ansi;
mod config;
mod discover;
mod http;
mod local;
mod providers;
mod report;
mod store;
mod tui;
mod types;

use anyhow::{Context, Result};
use chrono::{DateTime, Datelike, Duration, TimeZone, Utc};
use clap::{Args, Parser, Subcommand};
use config::Config;
use report::{Group, Period, QuotaStyle};
use types::*;

#[derive(Parser)]
#[command(
    name = "llmu",
    version,
    about = "Lean multi-provider LLM usage, cost, quota & balance reporter"
)]
struct Cli {
    /// Alternate config file (default: ~/.config/llmu/config.toml)
    #[arg(long, global = true)]
    config: Option<std::path::PathBuf>,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Write a sample config file
    Init,
    /// Show provider configuration status and capabilities
    Providers,
    /// Token / request / cost report with dynamic grouping & filters
    Usage(UsageArgs),
    /// Prepaid balances & credits (DeepSeek, Kimi, ...)
    Balance {
        #[arg(long)]
        json: bool,
    },
    /// Subscription quota / burn (local Claude Code window, GLM plan, ...)
    Quota {
        #[arg(long)]
        json: bool,
    },
    /// Live auto-refreshing dashboard (alias: watch)
    #[command(visible_alias = "watch")]
    Tui {
        #[arg(long, default_value = "30d")]
        since: String,
        /// Network refresh cadence in seconds (usage APIs, quotas, balances)
        #[arg(long, default_value_t = 60)]
        refresh: u64,
        /// Local-log refresh cadence in seconds (Claude Code / Codex JSONL)
        #[arg(long, default_value_t = 3)]
        local_refresh: u64,
    },
}

#[derive(Args)]
struct UsageArgs {
    /// Window start: 7d, 24h, mtd, wtd, or YYYY-MM-DD
    #[arg(long, default_value = "7d")]
    since: String,
    /// Window end: YYYY-MM-DD (default: now)
    #[arg(long)]
    until: Option<String>,
    /// Bucket: day | week | month
    #[arg(long, default_value = "day")]
    by: String,
    /// Comma list of extra group keys: provider,model,source
    #[arg(long, default_value = "provider")]
    group_by: String,
    /// Filter: comma list of providers (anthropic,openai,deepseek,kimi,glm,gemini)
    #[arg(long)]
    provider: Option<String>,
    /// Filter: substring match on model id
    #[arg(long)]
    model: Option<String>,
    /// Filter: api | local
    #[arg(long)]
    source: Option<String>,
    /// Emit aggregated rows as JSON (for scripting)
    #[arg(long)]
    json: bool,
}

pub(crate) fn parse_since(s: &str, now: DateTime<Utc>) -> Result<DateTime<Utc>> {
    let s = s.trim().to_ascii_lowercase();
    if s == "mtd" {
        return Utc
            .with_ymd_and_hms(now.year(), now.month(), 1, 0, 0, 0)
            .single()
            .context("bad date");
    }
    if s == "wtd" {
        let days = now.weekday().num_days_from_monday() as i64;
        let d = now.date_naive() - Duration::days(days);
        return Ok(Utc.from_utc_datetime(&d.and_hms_opt(0, 0, 0).unwrap()));
    }
    if let Some(n) = s.strip_suffix('d').and_then(|n| n.parse::<i64>().ok()) {
        return Ok(now - Duration::days(n));
    }
    if let Some(n) = s.strip_suffix('h').and_then(|n| n.parse::<i64>().ok()) {
        return Ok(now - Duration::hours(n));
    }
    let d = chrono::NaiveDate::parse_from_str(&s, "%Y-%m-%d").with_context(|| {
        format!("cannot parse --since '{s}' (use 7d, 24h, mtd, wtd, or YYYY-MM-DD)")
    })?;
    Ok(Utc.from_utc_datetime(&d.and_hms_opt(0, 0, 0).unwrap()))
}

struct Gathered {
    events: Vec<UsageEvent>,
    billed: Vec<BilledCost>,
    quotas: Vec<QuotaSnapshot>,
    balances: Vec<BalanceSnapshot>,
    notes: Vec<String>,
}

/// Everything one provider worker thread returns; the join loop merges
/// these into `Gathered`. Fresh quotas ride along in `to_cache` so the
/// last-known-good cache is written single-threaded after all joins —
/// `store::cache_quotas` is an unlocked read-modify-write.
#[derive(Default)]
struct ProviderFetch {
    events: Vec<UsageEvent>,
    billed: Vec<BilledCost>,
    quotas: Vec<QuotaSnapshot>,
    balances: Vec<BalanceSnapshot>,
    notes: Vec<String>,
    to_cache: Vec<(&'static str, Vec<QuotaSnapshot>)>,
}

/// Move one provider's usage fetch into the worker output. events and
/// billed replace whatever the worker had (there is only one usage call
/// per worker); notes append — a Fetch's notes carry provider diagnostics
/// (e.g. cost-report skips) that must not vanish.
fn absorb_fetch(out: &mut ProviderFetch, f: Fetch) {
    out.events = f.events;
    out.billed = f.billed;
    out.notes.extend(f.notes);
}

fn merge_provider(
    g: &mut Gathered,
    id: &str,
    res: std::thread::Result<ProviderFetch>,
    to_cache: &mut Vec<(&'static str, Vec<QuotaSnapshot>)>,
) {
    match res {
        Ok(f) => {
            g.events.extend(f.events);
            g.billed.extend(f.billed);
            g.quotas.extend(f.quotas);
            g.balances.extend(f.balances);
            g.notes.extend(f.notes);
            to_cache.extend(f.to_cache);
        }
        Err(_) => g.notes.push(format!("{id}: worker panicked")),
    }
}

/// Fetch all sources in parallel with plain OS threads — no async runtime
/// needed for a handful of REST calls.
pub(crate) fn gather(
    cfg: &Config,
    since: DateTime<Utc>,
    until: DateTime<Utc>,
    provider_filter: Option<&[String]>,
    want_usage: bool,
    want_quota: bool,
    want_balance: bool,
) -> Gathered {
    let mut g = Gathered {
        events: vec![],
        billed: vec![],
        quotas: vec![],
        balances: vec![],
        notes: vec![],
    };
    let provs = providers::all();
    let selected: Vec<&Box<dyn providers::Provider>> = provs
        .iter()
        .filter(|p| p.configured(cfg))
        .filter(|p| {
            provider_filter
                .map(|f| f.iter().any(|x| x == p.id()))
                .unwrap_or(true)
        })
        .collect();

    std::thread::scope(|s| {
        let mut handles = vec![];
        for p in &selected {
            handles.push((
                p.id(),
                s.spawn(move || {
                    let mut out = ProviderFetch::default();
                    if want_usage {
                        match p.usage(cfg, since, until) {
                            Ok(f) => absorb_fetch(&mut out, f),
                            Err(e) => out.notes.push(format!("{}: usage: {e}", p.id())),
                        }
                    }
                    if want_quota {
                        match p.quotas(cfg) {
                            Ok(q) => {
                                if !q.is_empty() {
                                    out.to_cache.push((p.id(), q.clone()));
                                }
                                out.quotas = q;
                            }
                            Err(e) => match store::cached_quotas(p.id()) {
                                Some(cached) => {
                                    out.notes.push(format!(
                                        "{}: quota: {e} — showing cached meters",
                                        p.id()
                                    ));
                                    out.quotas = cached;
                                }
                                None => out.notes.push(format!("{}: quota: {e}", p.id())),
                            },
                        }
                    }
                    if want_balance {
                        match p.balances(cfg) {
                            Ok(b) => out.balances = b,
                            Err(e) => out.notes.push(format!("{}: balance: {e}", p.id())),
                        }
                    }
                    out
                }),
            ));
        }

        // Local Claude Code logs in parallel with network fetches.
        let local_handle = if want_usage || want_quota {
            Some(s.spawn(move || local::claude_code::collect(cfg, since, until)))
        } else {
            None
        };

        let mut to_cache = vec![];
        for (id, h) in handles {
            merge_provider(&mut g, id, h.join(), &mut to_cache);
        }
        // Cache writes happen only here — after every worker has joined —
        // because cache_quotas does an unlocked read-modify-write.
        for (id, q) in to_cache {
            store::cache_quotas(id, &q);
        }
        if let Some(h) = local_handle {
            match h.join() {
                Ok(Ok(c)) => {
                    if want_quota {
                        if let Some(q) = local::claude_code::rolling_quota(&c.events, Utc::now()) {
                            g.quotas.push(q);
                        }
                    }
                    if want_usage {
                        g.events.extend(c.events);
                    }
                    g.notes.extend(c.notes);
                }
                Ok(Err(e)) => g.notes.push(format!("claude-code: {e}")),
                Err(_) => g.notes.push("claude-code: parser panicked".into()),
            }
        }
    });
    g
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let cfg = Config::load(cli.config.clone())?;
    let now = Utc::now();

    let Some(cmd) = cli.cmd else {
        return overview(&cfg, now);
    };
    match cmd {
        Cmd::Init => {
            let path = cli.config.unwrap_or_else(Config::default_path);
            if path.exists() {
                anyhow::bail!("{} already exists", path.display());
            }
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(&path, Config::sample())?;
            println!("wrote {}", path.display());
        }

        Cmd::Providers => {
            println!("{:<10} {:<11} capabilities", "provider", "configured");
            for p in providers::all() {
                println!(
                    "{:<10} {:<11} {}",
                    p.id(),
                    if p.configured(&cfg) { "yes" } else { "no" },
                    p.capabilities()
                );
            }
            println!(
                "{:<10} {:<11} subscription usage from local ~/.claude/projects JSONL transcripts",
                "claude-code",
                if cfg.claude_code.enabled { "yes" } else { "no" },
            );
            let mut prov = cfg.found.clone();
            prov.extend(discover::env_provenance());
            if !prov.is_empty() {
                println!("\nauto-detected credentials (read-only):");
                for (field, src) in &prov {
                    println!("  {field:<22} <- {src}");
                }
            }
        }

        Cmd::Usage(a) => {
            let since = parse_since(&a.since, now)?;
            let until = match &a.until {
                Some(u) => parse_since(u, now)? + Duration::days(1),
                None => now,
            };
            let period = Period::parse(&a.by).context("--by must be day|week|month")?;
            let groups: Vec<Group> = a
                .group_by
                .split(',')
                .filter(|s| !s.is_empty())
                .map(|s| Group::parse(s.trim()).with_context(|| format!("unknown group '{s}'")))
                .collect::<Result<_>>()?;
            let pf: Option<Vec<String>> = a
                .provider
                .as_ref()
                .map(|p| p.split(',').map(|s| s.trim().to_string()).collect());

            let mut g = gather(&cfg, since, until, pf.as_deref(), true, false, false);

            if let Some(m) = &a.model {
                let m = m.to_ascii_lowercase();
                g.events
                    .retain(|e| e.model.to_ascii_lowercase().contains(&m));
            }
            if let Some(src) = &a.source {
                let want = match src.as_str() {
                    "api" => SourceKind::Api,
                    "local" => SourceKind::LocalLogs,
                    _ => anyhow::bail!("--source must be api|local"),
                };
                g.events.retain(|e| e.source == want);
            }

            let rows = report::aggregate(&g.events, period, &groups);
            if a.json {
                println!("{}", serde_json::to_string_pretty(&rows)?);
            } else {
                print!(
                    "{}",
                    ansi::table(&report::render_table(period, &groups, &rows))
                );
                // billed (authoritative) summary, kept separate from estimates
                if !g.billed.is_empty() {
                    let mut by_prov: std::collections::BTreeMap<String, f64> = Default::default();
                    for b in &g.billed {
                        *by_prov.entry(b.provider.clone()).or_default() += b.amount_usd;
                    }
                    println!(
                        "\nBilled (provider cost APIs, {} → {}):",
                        since.format("%Y-%m-%d"),
                        now.format("%Y-%m-%d")
                    );
                    for (p, amt) in by_prov {
                        println!("  {p:<10} ${amt:.2}");
                    }
                }
            }
            for n in &g.notes {
                eprintln!("note: {n}");
            }
        }

        Cmd::Balance { json } => {
            let g = gather(&cfg, now, now, None, false, false, true);
            store::record_balances(&g.balances).ok();
            if json {
                println!("{}", serde_json::to_string_pretty(&g.balances)?);
            } else if g.balances.is_empty() {
                println!("no balance sources configured (deepseek / kimi)");
            } else {
                println!(
                    "{:<10} {:>12} {:>12} {:>12}  currency",
                    "provider", "total", "granted", "topped-up"
                );
                for b in &g.balances {
                    println!(
                        "{:<10} {:>12.2} {:>12.2} {:>12.2}  {}",
                        b.provider, b.total, b.granted, b.topped_up, b.currency
                    );
                }
            }
            for n in &g.notes {
                eprintln!("note: {n}");
            }
        }

        Cmd::Quota { json } => {
            let since = now - Duration::hours(6);
            let g = gather(&cfg, since, now, None, false, true, false);
            if json {
                println!("{}", serde_json::to_string_pretty(&g.quotas)?);
            } else if g.quotas.is_empty() {
                println!("no quota data available");
            } else {
                for q in &g.quotas {
                    println!("{}", report::render_quota(q, QuotaStyle::Command));
                }
            }
            for n in &g.notes {
                eprintln!("note: {n}");
            }
        }

        Cmd::Tui {
            since,
            refresh,
            local_refresh,
        } => {
            parse_since(&since, now)?; // validate the spec up front
            tui::run(cfg, since, refresh, local_refresh)?;
        }
    }
    Ok(())
}

/// Bare `llmu`: the zero-config landing view. Auto-discovers whatever
/// credentials and local logs exist and shows quotas, balances, and a
/// 7-day per-provider summary in one shot.
fn overview(cfg: &Config, now: DateTime<Utc>) -> Result<()> {
    let n_conf = providers::all()
        .iter()
        .filter(|p| p.configured(cfg))
        .count()
        + usize::from(cfg.claude_code.enabled);
    println!("llmu — {} source(s) detected (run `llmu providers` for details, `llmu --help` for filters)\n", n_conf);

    let since = now - Duration::days(7);
    let g = gather(cfg, since, now, None, true, true, true);

    if !g.quotas.is_empty() {
        println!("{}", ansi::paint("subscription quotas:", ansi::BOLD));
        for q in &g.quotas {
            println!("{}", report::render_quota(q, QuotaStyle::Overview));
        }
        println!();
    }

    if !g.balances.is_empty() {
        println!("{}", ansi::paint("balances:", ansi::BOLD));
        for b in &g.balances {
            println!(
                "  {} {}  ({:.2} granted / {:.2} topped-up)  [{}]",
                ansi::paint(
                    &format!("{:<8}", b.provider),
                    ansi::provider_color(&b.provider)
                ),
                ansi::paint(&format!("{:>10.2} total", b.total), ansi::GREEN),
                b.granted,
                b.topped_up,
                b.currency
            );
        }
        println!();
    }

    if g.events.is_empty() {
        println!("no usage in the last 7 days (or no usage-capable source detected).");
    } else {
        println!("{}", ansi::paint("last 7 days by provider:", ansi::BOLD));
        let rows = report::aggregate(&g.events, Period::Day, &[Group::Provider]);
        print!(
            "{}",
            ansi::table(&report::render_table(
                Period::Day,
                &[Group::Provider],
                &rows
            ))
        );
        println!("{}", ansi::paint(&usage_scope_line(&g, cfg), ansi::DIM));
        if !g.billed.is_empty() {
            let sum: f64 = g.billed.iter().map(|b| b.amount_usd).sum();
            println!(
                "\n{}",
                ansi::paint(
                    &format!("billed (authoritative, provider cost APIs): ${sum:.2}"),
                    ansi::GREEN
                )
            );
        }
    }
    for n in &g.notes {
        eprintln!("note: {n}");
    }
    Ok(())
}

/// One dim line stating exactly which sources feed the usage numbers and
/// which configured providers can't (quota/balance only) — the usage
/// panels sum ONLY event-producing sources.
fn usage_scope_line(g: &Gathered, cfg: &Config) -> String {
    use std::collections::BTreeSet;
    let mut feeds: BTreeSet<String> = BTreeSet::new();
    for e in &g.events {
        let src = match e.source {
            SourceKind::Api => "api",
            SourceKind::LocalLogs => "local logs",
        };
        feeds.insert(format!("{} ({src})", e.provider));
    }
    let mut no_feed: Vec<&str> = vec![];
    for p in providers::all() {
        if p.configured(cfg) && !g.events.iter().any(|e| e.provider == p.id()) {
            match p.id() {
                "deepseek" => no_feed.push("deepseek (balance only)"),
                "kimi" => no_feed.push("kimi (quota only)"),
                "glm" => {
                    if !feeds.iter().any(|f| f.starts_with("glm")) {
                        no_feed.push("glm (quota only)")
                    }
                }
                "claude" => no_feed.push("claude (quota only)"),
                _ => {}
            }
        }
    }
    let mut s = format!(
        "usage counted from: {}",
        if feeds.is_empty() {
            "no event sources".into()
        } else {
            feeds.into_iter().collect::<Vec<_>>().join(", ")
        }
    );
    if !no_feed.is_empty() {
        s.push_str(&format!(
            "  •  not countable (no usage feed): {}",
            no_feed.join(", ")
        ));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_gathered() -> Gathered {
        Gathered {
            events: vec![],
            billed: vec![],
            quotas: vec![],
            balances: vec![],
            notes: vec![],
        }
    }

    fn quota(provider: &str) -> QuotaSnapshot {
        QuotaSnapshot {
            provider: provider.into(),
            plan: "pro".into(),
            window: "5h".into(),
            used: 10.0,
            limit: 100.0,
            unit: "%".into(),
            resets_at: None,
        }
    }

    /// A panicking provider worker must surface as a note, not vanish —
    /// the review found the join loop silently dropping JoinError.
    #[test]
    fn panicked_provider_thread_yields_note() {
        let h = std::thread::spawn(|| -> ProviderFetch { panic!("boom") });
        let mut g = empty_gathered();
        let mut to_cache = vec![];
        merge_provider(&mut g, "deepseek", h.join(), &mut to_cache);
        assert_eq!(g.notes, vec!["deepseek: worker panicked".to_string()]);
        assert!(g.quotas.is_empty());
        assert!(to_cache.is_empty());
    }

    fn event(provider: &str) -> UsageEvent {
        UsageEvent {
            provider: provider.into(),
            source: SourceKind::Api,
            model: "glm-4.6".into(),
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

    fn billed(provider: &str) -> BilledCost {
        BilledCost {
            provider: provider.into(),
            start: Utc::now(),
            amount_usd: 0.42,
            description: "cost report".into(),
        }
    }

    /// Provider notes (e.g. cost-report diagnostics) must survive the Ok
    /// arm of the usage fetch — events and billed land by assignment,
    /// notes append to whatever the worker already accumulated.
    #[test]
    fn absorb_fetch_moves_events_billed_and_notes() {
        let f = Fetch {
            events: vec![event("glm")],
            billed: vec![billed("glm")],
            notes: vec!["quota row skipped".into()],
        };
        let mut out = ProviderFetch {
            notes: vec!["pre-existing".into()],
            ..Default::default()
        };
        absorb_fetch(&mut out, f);
        assert_eq!(out.events.len(), 1);
        assert_eq!(out.events[0].provider, "glm");
        assert_eq!(out.billed.len(), 1);
        assert_eq!(out.billed[0].amount_usd, 0.42);
        assert_eq!(
            out.notes,
            vec!["pre-existing".to_string(), "quota row skipped".to_string()]
        );
    }

    #[test]
    fn merge_collects_results_and_defers_cache_writes() {
        let f = ProviderFetch {
            quotas: vec![quota("glm")],
            to_cache: vec![("glm", vec![quota("glm")])],
            notes: vec!["glm: note".into()],
            ..Default::default()
        };
        let mut g = empty_gathered();
        let mut to_cache = vec![];
        merge_provider(&mut g, "glm", Ok(f), &mut to_cache);
        assert_eq!(g.quotas.len(), 1);
        assert_eq!(g.notes, vec!["glm: note".to_string()]);
        assert_eq!(to_cache.len(), 1);
        assert_eq!(to_cache[0].0, "glm");
    }

    /// AC-3: each API-backed provider file opens with a doc block that
    /// names its base URL and auth shape.
    fn check_doc_header(path: &str, src: &str, base: &str, auth: &str) {
        let first = src
            .lines()
            .find(|l| !l.trim().is_empty())
            .expect("file has no non-empty lines");
        assert!(
            first.trim_start().starts_with("//!") || first.trim_start().starts_with("///"),
            "{path}: first non-empty line must be a doc comment, got {first:?}"
        );
        let head = src.lines().take(20).collect::<Vec<_>>().join("\n");
        assert!(
            head.contains(base),
            "{path}: first 20 lines must mention {base}"
        );
        assert!(
            head.contains(auth),
            "{path}: first 20 lines must mention auth marker {auth:?}"
        );
    }

    #[test]
    fn provider_doc_headers_lead_with_doc_comment() {
        check_doc_header(
            "providers/anthropic.rs",
            include_str!("providers/anthropic.rs"),
            "api.anthropic.com",
            "x-api-key",
        );
        check_doc_header(
            "providers/deepseek.rs",
            include_str!("providers/deepseek.rs"),
            "api.deepseek.com",
            "Bearer",
        );
        check_doc_header(
            "providers/openai.rs",
            include_str!("providers/openai.rs"),
            "api.openai.com",
            "Bearer",
        );
    }
}
