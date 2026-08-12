mod ansi;
mod config;
mod credentials;
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
use providers::{FetchContext, QuotaFetch};
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
    /// Bypass the optional HTTP response cache (FR-3.2): fetch live and
    /// store successful responses. For `llmu --fresh tui`, bypasses only
    /// the initial full network fetch; later scheduled ticks honor the TTL.
    #[arg(long, global = true)]
    fresh: bool,
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
        /// Emit rows as CSV (RFC 4180, UTF-8, LF)
        #[arg(long, conflicts_with = "json")]
        csv: bool,
        /// Offline daily balance history from the local balances.jsonl
        /// (no provider requests, no snapshot append)
        #[arg(long)]
        history: bool,
    },
    /// Subscription quota / burn (local Claude Code window, GLM plan, ...)
    Quota {
        #[arg(long)]
        json: bool,
        /// Emit rows as CSV (RFC 4180, UTF-8, LF)
        #[arg(long, conflicts_with = "json")]
        csv: bool,
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
    /// Filter: comma list of providers (anthropic,openai,deepseek,kimi,glm,gemini,qwen)
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
    /// Emit aggregated rows as CSV (RFC 4180, UTF-8, LF)
    #[arg(long, conflicts_with = "json")]
    csv: bool,
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

/// Move one provider's quota fetch into the worker output. Rows replace
/// whatever the worker had; notes append — QuotaFetch notes carry quota
/// diagnostics that must not vanish. The last-known-good cache write is
/// queued only for nonempty snapshots observed live this call
/// (`refresh_last_known_good`, AD-1 / FR-3.10): cached-origin rows must
/// never re-age the cache timestamp.
fn absorb_quota(out: &mut ProviderFetch, id: &'static str, f: QuotaFetch) {
    out.notes.extend(f.notes);
    out.quotas = f.snapshots;
    if !out.quotas.is_empty() && f.refresh_last_known_good {
        out.to_cache.push((id, out.quotas.clone()));
    }
}

/// Quota failure path: serve the last-known-good meters (age-labeled)
/// when a cache entry exists, else a plain error note. Cached rows are
/// cached origin and never queue another cache write.
fn quota_failure(
    out: &mut ProviderFetch,
    id: &'static str,
    e: anyhow::Error,
    cached: Option<Vec<QuotaSnapshot>>,
) {
    match cached {
        Some(cached) => {
            out.notes
                .push(format!("{id}: quota: {e} — showing cached meters"));
            out.quotas = cached;
        }
        None => out.notes.push(format!("{id}: quota: {e}")),
    }
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

/// One shared `--provider` filter predicate (FR-6.1): a None filter admits
/// every provider; a Some filter admits exactly the listed ids. Provider
/// worker selection and the local Claude Code event path both go through
/// it, so filtered-out routed rows (e.g. qwen* models served through an
/// Anthropic-compatible route) never leak into reports or the rolling
/// Claude quota.
fn filter_admits(filter: Option<&[String]>, provider: &str) -> bool {
    filter
        .map(|f| f.iter().any(|x| x == provider))
        .unwrap_or(true)
}

/// Fetch all sources in parallel with plain OS threads — no async runtime
/// needed for a handful of REST calls. `ctx` threads the raw HTTP cache
/// options and per-invocation freshness into every provider method
/// (FR-3.2, plan Task 7: no hidden global freshness state).
#[allow(clippy::too_many_arguments)] // explicit context threading by design (plan Task 7)
pub(crate) fn gather(
    cfg: &Config,
    ctx: &FetchContext,
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
        .filter(|p| filter_admits(provider_filter, p.id()))
        .collect();

    std::thread::scope(|s| {
        let mut handles = vec![];
        for p in &selected {
            handles.push((
                p.id(),
                s.spawn(move || {
                    let mut out = ProviderFetch::default();
                    if want_usage {
                        match p.usage(cfg, ctx, since, until) {
                            Ok(f) => absorb_fetch(&mut out, f),
                            Err(e) => out.notes.push(format!("{}: usage: {e}", p.id())),
                        }
                    }
                    if want_quota {
                        match p.quotas(cfg, ctx) {
                            Ok(f) => absorb_quota(&mut out, p.id(), f),
                            Err(e) => {
                                quota_failure(&mut out, p.id(), e, store::cached_quotas(p.id()))
                            }
                        }
                    }
                    if want_balance {
                        match p.balances(cfg, ctx) {
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
                    // FR-6.1: `--provider` filters local Claude Code events
                    // exactly like Provider::usage events — routed qwen*
                    // rows are admitted only by a qwen filter, and rows
                    // from excluded providers never feed the rolling
                    // Claude quota or the usage report.
                    let mut local_events = c.events;
                    local_events.retain(|e| filter_admits(provider_filter, &e.provider));
                    if want_quota {
                        if let Some(q) =
                            local::claude_code::rolling_quota(&local_events, Utc::now())
                        {
                            g.quotas.push(q);
                        }
                    }
                    if want_usage {
                        g.events.extend(local_events);
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
    let ctx = FetchContext::from_config(&cfg, cli.fresh);
    let now = Utc::now();

    let Some(cmd) = cli.cmd else {
        return overview(&cfg, &ctx, now);
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
                println!("\nauto-detected credentials (read-only except supported OAuth refresh):");
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

            let mut g = gather(&cfg, &ctx, since, until, pf.as_deref(), true, false, false);

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
            } else if a.csv {
                print!("{}", report::render_usage_csv(&groups, &rows));
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

        Cmd::Balance { json, csv, history } => handle_balance(&cfg, &ctx, now, json, csv, history)?,

        Cmd::Quota { json, csv } => {
            let since = now - Duration::hours(6);
            let g = gather(&cfg, &ctx, since, now, None, false, true, false);
            if json {
                println!("{}", serde_json::to_string_pretty(&g.quotas)?);
            } else if csv {
                print!("{}", report::render_quota_csv(&g.quotas));
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
            tui::run(cfg, since, refresh, local_refresh, cli.fresh)?;
        }
    }
    Ok(())
}

/// `llmu balance` dispatch. The `--history` branch (FR-2.1) reads only
/// the offline `llmu/balances.jsonl` store: it makes no provider requests
/// and appends no snapshot. The ordinary branch snapshots fetched
/// balances and surfaces write failures instead of discarding them
/// (FR-2.9).
fn handle_balance(
    cfg: &Config,
    ctx: &FetchContext,
    now: DateTime<Utc>,
    json: bool,
    csv: bool,
    history: bool,
) -> Result<()> {
    if history {
        let hist = match store::read_balance_history() {
            Ok(h) => h,
            Err(e) => {
                eprintln!("note: cannot read balance history: {e}");
                return Ok(());
            }
        };
        if hist.skipped > 0 {
            eprintln!(
                "note: skipped {} malformed balance-history record(s)",
                hist.skipped
            );
        }
        print!("{}", history_payload(&hist, json, csv)?);
        return Ok(());
    }
    let mut g = gather(cfg, ctx, now, now, None, false, false, true);
    if let Err(e) = store::record_balances(&g.balances) {
        g.notes
            .push(format!("failed to append balance snapshot: {e}"));
    }
    if json {
        println!("{}", serde_json::to_string_pretty(&g.balances)?);
    } else if csv {
        print!("{}", report::render_balance_csv(&g.balances));
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
    Ok(())
}

/// The complete stdout payload for `balance --history` (FR-2.7): JSON
/// emits the normalized rows as an array, CSV the same rows as a
/// spreadsheet, and the table renders the same rows. Malformed-record
/// counts are a stderr note, never stdout. Empty results keep the
/// header in CSV mode and the human empty message otherwise (FR-1.7).
fn history_payload(hist: &store::BalanceHistory, json: bool, csv: bool) -> Result<String> {
    if json {
        Ok(serde_json::to_string_pretty(&hist.rows)? + "\n")
    } else if csv {
        Ok(report::render_balance_history_csv(&hist.rows))
    } else if hist.rows.is_empty() {
        Ok("no balance history (llmu/balances.jsonl missing or empty)\n".to_string())
    } else {
        Ok(ansi::table(&report::render_balance_history(&hist.rows)))
    }
}

/// Bare `llmu`: the zero-config landing view. Auto-discovers whatever
/// credentials and local logs exist and shows quotas, balances, and a
/// 7-day per-provider summary in one shot.
fn overview(cfg: &Config, ctx: &FetchContext, now: DateTime<Utc>) -> Result<()> {
    let n_conf = providers::all()
        .iter()
        .filter(|p| p.configured(cfg))
        .count()
        + usize::from(cfg.claude_code.enabled);
    println!("llmu — {} source(s) detected (run `llmu providers` for details, `llmu --help` for filters)\n", n_conf);

    let since = now - Duration::days(7);
    let g = gather(cfg, ctx, since, now, None, true, true, true);

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
                "qwen" => no_feed.push("qwen (no local usage records yet)"),
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

    /// FR-6.1: the same `--provider` predicate that selects provider
    /// workers also admits local Claude Code events — None admits all,
    /// and a qwen filter admits qwen while excluding every other id.
    #[test]
    fn provider_filter_admits_shared_with_local_events() {
        let qwen_only = Some(vec!["qwen".to_string(), "glm".to_string()]);
        assert!(filter_admits(qwen_only.as_deref(), "qwen"));
        assert!(filter_admits(qwen_only.as_deref(), "glm"));
        assert!(!filter_admits(qwen_only.as_deref(), "anthropic"));
        assert!(filter_admits(None, "anthropic"));
        assert!(filter_admits(None, "qwen"));
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

    /// Task 7 (RED): one-shot commands derive the typed fetch context
    /// from `[http_cache]` plus the global `--fresh` flag (FR-3.2).
    #[test]
    fn fresh_context_builds_from_config_and_flag() {
        let mut cfg = Config::default();
        cfg.http_cache.ttl_seconds = 60;
        let ctx = providers::FetchContext::from_config(&cfg, true);
        assert!(ctx.fresh);
        assert_eq!(ctx.cache.ttl_seconds, 60);
        assert!(!providers::FetchContext::from_config(&cfg, false).fresh);
    }

    /// Task 7 (RED): every one-shot gather call site (usage, quota,
    /// balance, overview) passes the shared typed fetch context — no
    /// hidden global or environment freshness state (plan Task 7).
    #[test]
    fn one_shot_gather_sites_pass_the_fetch_context() {
        let needle = ["gat", "her("].concat();
        let src = include_str!("main.rs");
        // Only the non-test half of the file: the tests module itself
        // legitimately contains the needle (its own assertions).
        let prod = src.split("#[cfg(test)]").next().unwrap();
        let sites: Vec<&str> = prod
            .lines()
            .filter(|l| l.contains(&needle) && !l.trim_start().starts_with("pub(crate) fn gather"))
            .collect();
        assert_eq!(sites.len(), 4, "usage, quota, balance, and overview");
        for line in &sites {
            assert!(
                line.contains("ctx"),
                "every one-shot gather must pass the context, got: {line}"
            );
        }
    }

    /// Task 7 (RED): `--fresh` also reaches the TUI as its initial-fetch
    /// bypass (FR-3.2: later scheduled ticks honor the TTL).
    #[test]
    fn tui_run_receives_the_global_fresh_flag() {
        let src = include_str!("main.rs");
        assert!(
            src.contains("cli.fresh"),
            "the global --fresh must reach the TUI dispatch"
        );
    }

    /// Task 7 (RED): a panicking provider worker must surface as a note, not vanish —
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

    /// Default/empty quota results must never queue a last-known-good
    /// cache write (FR-3.10: empty results set the marker false).
    #[test]
    fn absorb_quota_empty_default_never_queues_cache() {
        let mut out = ProviderFetch::default();
        absorb_quota(&mut out, "claude", QuotaFetch::default());
        assert!(out.quotas.is_empty());
        assert!(out.to_cache.is_empty());
        assert!(out.notes.is_empty());
    }

    /// Live nonempty rows land in the output AND queue the cache write.
    #[test]
    fn absorb_quota_live_rows_land_and_queue_cache() {
        let mut out = ProviderFetch::default();
        absorb_quota(&mut out, "glm", QuotaFetch::live(vec![quota("glm")]));
        assert_eq!(out.quotas.len(), 1);
        assert_eq!(out.to_cache.len(), 1);
        assert_eq!(out.to_cache[0].0, "glm");
        assert_eq!(out.to_cache[0].1.len(), 1);
    }

    /// Cached-origin rows (marker false — e.g. Task 7 raw TTL hits) land
    /// but never queue a cache write: a TTL hit must not re-age the
    /// last-known-good data.
    #[test]
    fn absorb_quota_cached_origin_rows_never_queue_cache() {
        let mut out = ProviderFetch::default();
        let f = QuotaFetch {
            snapshots: vec![quota("claude")],
            notes: vec![],
            refresh_last_known_good: false,
        };
        absorb_quota(&mut out, "claude", f);
        assert_eq!(out.quotas.len(), 1);
        assert!(out.to_cache.is_empty());
    }

    /// Provider diagnostic notes (e.g. skipped rows) survive into the
    /// worker output alongside quota rows.
    #[test]
    fn absorb_quota_appends_provider_notes() {
        let mut out = ProviderFetch {
            notes: vec!["pre-existing".into()],
            ..Default::default()
        };
        let f = QuotaFetch {
            snapshots: vec![],
            notes: vec!["quota row skipped".into()],
            refresh_last_known_good: true,
        };
        absorb_quota(&mut out, "claude", f);
        assert_eq!(
            out.notes,
            vec!["pre-existing".to_string(), "quota row skipped".to_string()]
        );
    }

    /// The existing error fallback stays: cached rows win with the age
    /// label, and — being cached origin — never queue a refresh.
    #[test]
    fn quota_failure_falls_back_to_cached_quotas_with_label() {
        let mut out = ProviderFetch::default();
        let cached = vec![quota("claude")];
        quota_failure(
            &mut out,
            "claude",
            anyhow::anyhow!(
                "oauth/usage rate-limited (this endpoint throttles hard) — retry in a few minutes"
            ),
            Some(cached),
        );
        assert_eq!(out.quotas.len(), 1);
        assert_eq!(
            out.notes,
            vec![
                "claude: quota: oauth/usage rate-limited (this endpoint throttles hard) — retry in a few minutes — showing cached meters".to_string()
            ]
        );
        assert!(out.to_cache.is_empty());
    }

    /// Without a cache entry the error surfaces as a plain note and no
    /// rows or cache writes are produced.
    #[test]
    fn quota_failure_without_cache_emits_error_note_only() {
        let mut out = ProviderFetch::default();
        quota_failure(
            &mut out,
            "glm",
            anyhow::anyhow!("GLM key is valid but has no Coding Plan subscription"),
            None,
        );
        assert_eq!(
            out.notes,
            vec!["glm: quota: GLM key is valid but has no Coding Plan subscription".to_string()]
        );
        assert!(out.quotas.is_empty());
        assert!(out.to_cache.is_empty());
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

    // -------------------------------------------------------------------
    // Offline balance history dispatch (FR-2, FR-2.9): RED contracts.
    // -------------------------------------------------------------------

    fn hist_rows() -> Vec<BalanceHistoryRow> {
        vec![BalanceHistoryRow {
            from: chrono::NaiveDate::from_ymd_opt(2026, 8, 1).unwrap(),
            to: chrono::NaiveDate::from_ymd_opt(2026, 8, 2).unwrap(),
            provider: "deepseek".into(),
            currency: "USD".into(),
            opening: 10.0,
            closing: 7.5,
            spent: 2.5,
            funded: 0.0,
        }]
    }

    #[test]
    fn balance_history_json_payload_is_an_array_of_rows() {
        let hist = store::BalanceHistory {
            rows: hist_rows(),
            skipped: 2,
        };
        let payload = history_payload(&hist, true, false).unwrap();
        let v: serde_json::Value = serde_json::from_str(&payload).expect("valid JSON payload");
        let rows = v.as_array().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["from"], "2026-08-01");
        assert_eq!(rows[0]["to"], "2026-08-02");
        assert_eq!(rows[0]["provider"], "deepseek");
        assert_eq!(rows[0]["spent"], 2.5);
        assert!(
            !payload.contains("skipped"),
            "the malformed-record count is a stderr note, never part of JSON"
        );
    }

    #[test]
    fn balance_history_empty_payload_prints_empty_message() {
        let hist = store::BalanceHistory {
            rows: vec![],
            skipped: 0,
        };
        assert_eq!(
            history_payload(&hist, false, false).unwrap(),
            "no balance history (llmu/balances.jsonl missing or empty)\n"
        );
        assert_eq!(history_payload(&hist, true, false).unwrap(), "[]\n");
    }

    #[test]
    fn balance_history_table_payload_renders_the_rows() {
        let hist = store::BalanceHistory {
            rows: hist_rows(),
            skipped: 0,
        };
        let payload = history_payload(&hist, false, false).unwrap();
        assert!(payload.contains("2026-08-01"));
        assert!(payload.contains("7.50"));
        assert!(payload.contains("2.50"));
    }

    /// The history branch reads the store and returns before any network
    /// gather or snapshot append can run (FR-2.1: no provider requests,
    /// no append).
    #[test]
    fn balance_history_dispatch_never_gathers_or_appends() {
        let src = include_str!("main.rs");
        let after = src.split("if history {").nth(1).expect("history branch");
        let branch = after.split("return Ok(())").next().expect("branch end");
        assert!(
            branch.contains("store::read_balance_history"),
            "the history branch must read the offline store"
        );
        assert!(
            !branch.contains("gather("),
            "the history branch must never call gather (no network)"
        );
        assert!(
            !branch.contains("record_balances("),
            "the history branch must never append a snapshot"
        );
    }

    /// FR-2.9: ordinary `llmu balance` snapshot-write failures surface as
    /// a note instead of the old silent `.ok()` discard. The needle is
    /// built by concatenation so this test's own source cannot satisfy it.
    #[test]
    fn balance_snapshot_write_failures_are_surfaced() {
        let src = include_str!("main.rs");
        assert!(
            src.contains("failed to append balance snapshot"),
            "snapshot-write failures must carry a note (FR-2.9)"
        );
        let discard = ["record_balances(&g.balances)", ".ok()"].concat();
        assert!(
            !src.contains(&discard),
            "the silent discard must be gone (FR-2.9)"
        );
    }

    // -------------------------------------------------------------------
    // CSV output dispatch (FR-1, Task 8): RED clap conflict and payload
    // contracts.
    // -------------------------------------------------------------------

    /// FR-1.2: clap itself rejects `--json --csv` on every report
    /// command — before any config/provider read or store mutation.
    #[test]
    fn csv_conflicts_with_json_at_parse_time() {
        let cases: &[&[&str]] = &[
            &["llmu", "usage", "--json", "--csv"],
            &["llmu", "usage", "--csv", "--json"],
            &["llmu", "balance", "--json", "--csv"],
            &["llmu", "balance", "--history", "--json", "--csv"],
            &["llmu", "quota", "--csv", "--json"],
        ];
        for args in cases {
            let err = Cli::try_parse_from(*args)
                .err()
                .expect("--json --csv must be rejected by clap");
            assert!(
                err.to_string().contains("cannot be used with"),
                "--json --csv must be a clap conflict, got: {err}"
            );
        }
    }

    /// FR-1.1: `--csv` parses on usage, balance, quota; `--history`
    /// combines freely with `--csv`.
    #[test]
    fn csv_flags_parse_on_every_report_command() {
        let usage =
            Cli::try_parse_from(["llmu", "usage", "--csv"]).expect("usage --csv must parse");
        let Some(Cmd::Usage(a)) = usage.cmd else {
            panic!("expected the usage subcommand")
        };
        assert!(a.csv && !a.json);
        let balance = Cli::try_parse_from(["llmu", "balance", "--history", "--csv"])
            .expect("balance --history --csv must parse");
        let Some(Cmd::Balance { csv, json, history }) = balance.cmd else {
            panic!("expected the balance subcommand")
        };
        assert!(csv && history && !json);
        let quota =
            Cli::try_parse_from(["llmu", "quota", "--csv"]).expect("quota --csv must parse");
        let Some(Cmd::Quota { csv, json }) = quota.cmd else {
            panic!("expected the quota subcommand")
        };
        assert!(csv && !json);
    }

    /// The rejection must happen before `Config::load`, which precedes
    /// every provider read, network call, and balance-store mutation.
    #[test]
    fn csv_conflict_is_rejected_before_config_or_provider_work() {
        let src = include_str!("main.rs");
        let prod = src.split("#[cfg(test)]").next().unwrap();
        let parse = prod
            .find("Cli::parse()")
            .expect("main must parse the CLI first");
        let config = prod
            .find("Config::load")
            .expect("main must load the config");
        assert!(
            parse < config,
            "clap must reject --json --csv before any config/provider read (FR-1.2)"
        );
    }

    /// FR-2.7/FR-1.7: `balance --history --csv` renders the same
    /// normalized rows as JSON/table, and empty history still emits the
    /// header.
    #[test]
    fn history_csv_payload_emits_rows_or_header() {
        let hist = store::BalanceHistory {
            rows: hist_rows(),
            skipped: 0,
        };
        assert_eq!(
            history_payload(&hist, false, true).unwrap(),
            "from,to,provider,currency,opening,closing,spent,funded\n\
             2026-08-01,2026-08-02,deepseek,USD,10,7.5,2.5,0\n"
        );
        let empty = store::BalanceHistory {
            rows: vec![],
            skipped: 0,
        };
        assert_eq!(
            history_payload(&empty, false, true).unwrap(),
            "from,to,provider,currency,opening,closing,spent,funded\n"
        );
    }
}
