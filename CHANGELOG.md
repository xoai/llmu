# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.5] - 2026-08-14

### Added

- OpenCode local usage: read-only, WAL-aware reads of
  `~/.local/share/opencode/opencode.db` (`OPENCODE_DATA_DIR` override,
  `XDG_DATA_HOME` fallback) surface completed assistant records for Qwen,
  GLM, DeepSeek, Kimi, and OpenAI under llmu's canonical provider ids —
  closing the observed gap where Qwen usage ran through OpenCode but never
  appeared. Records are additive with API and client-log sources (possible
  overlap, no cross-source dedup), reasoning folds into output, cache maps
  directly, and only llmu `[pricing]` estimates appear — never OpenCode's
  local cost. Attribution is strict: an exact allowlisted provider-id set,
  never model-prefix inference.
- Standalone `opencode` row in `llmu providers`: `yes` only when a
  read-only one-row query finds a record accepted by the same strict
  validator as collection; missing, busy, corrupt, or incompatible
  databases answer `no` with no diagnostics in the status table.
- Token totals saturate at `u64::MAX` across aggregation, reports, and the
  TUI, so valid maximum-value OpenCode records never panic or wrap.

## [0.1.4] - 2026-08-12

### Fixed

- Live watch mode now preserves Claude Code, Codex, Gemini, and Qwen local
  usage across the 3-second local refresh, including activity, daily token,
  and per-model rows.

## [0.1.3] - 2026-08-12

### Added

- Qwen (Alibaba Cloud Model Studio / QwenCloud) provider: read-only local
  usage from Qwen Code's request token ledger
  (`usage/token-usage-YYYY-MM.jsonl`, writer-local months, RFC3339
  timestamps, id-deduplicated) and legacy session summaries
  (`usage_record.jsonl`, last-wins per session, request-ledger sessions
  suppress the legacy summary). Routed Claude Code `qwen*` transcript rows
  are attributed to `qwen` and stay additive — no cross-client
  deduplication; `--provider qwen` includes both.
- `[qwen]` configuration and discovery: three non-interchangeable key
  classes — standard (`DASHSCOPE_API_KEY`, then `BAILIAN_API_KEY`), Coding
  Plan (`BAILIAN_CODING_PLAN_API_KEY`), and Token Plan
  (`BAILIAN_TOKEN_PLAN_API_KEY`) — plus Qwen Code settings/home discovery
  (`[qwen].home` > `QWEN_HOME` > `~/.qwen`) and runtime precedence
  (`[qwen].runtime_dir` > `QWEN_RUNTIME_DIR` > settings
  `advanced.runtimeOutputDir` > effective Qwen home). An `sk-sp-*` prefix
  never identifies a plan class.
- Honest Qwen availability boundary: llmu adds no built-in Qwen price
  guesses, performs no Qwen network call, and never uses browser cookies
  or a console `sec_token`; QwenCloud account analytics, quota, and
  billing remain console-only, so a key-only setup is reported as
  configured without claiming live account usage.

## [0.1.2] - 2026-08-11

### Fixed

- Live watch mode now renders quota rows beyond the first six when terminal
  space permits; compact terminals report the number of additional rows
  instead of silently hiding later providers such as GLM.

## [0.1.1] - 2026-08-11

### Added

- Gemini Code Assist quotas: plaintext `oauth_creds.json` discovery,
  five-minute Google OAuth refresh with rotation-safe persistence,
  `loadCodeAssist` + the corrected `v1internal:retrieveUserQuota`
  endpoint, and project precedence (config > `GOOGLE_CLOUD_PROJECT` >
  `GOOGLE_CLOUD_PROJECT_ID`). Encrypted/keychain stores are diagnosed as
  unsupported and never mutated; llmu never onboards accounts.
- Claude Code OAuth auto-refresh: proactive five-minute refresh, rotated
  refresh-token persistence, one reactive 401 retry, transient-failure
  fallback to the still-valid access token, and refresh-token-expiry
  warnings. Direct/OpenCode access-only tokens are never refreshed.
- `llmu balance --history`: offline daily spend derivation from the
  `balances.jsonl` snapshots (UTC daily closes, visible gaps,
  spent/funded deltas, deterministic ordering) with no provider requests
  and no snapshot append.
- `--csv` output for `usage`, `balance`, `quota`, and `balance --history`
  (RFC 4180, UTF-8, stable schemas, empty-result headers); `--json --csv`
  is rejected by clap before any work begins.
- Optional HTTP response cache: `[http_cache] ttl_seconds` (zero default
  disables it) caches eligible successful GET/JSON responses under
  authorization-bound SHA-256 keys; OAuth exchanges, POSTs, and local-file
  reads are never cached.
- Global `--fresh` flag: bypasses raw cache reads for one-shot commands
  (TUI: initial fetch only; each `r` keypress bypasses exactly one full
  refresh), and raw cache hits never re-age the last-known-good quota
  cache.

### Security

- Credential mutation stays read-only except validated plaintext
  Gemini/Claude OAuth refresh: unknown fields preserved, mode `0600`,
  llmu-only lock plus compare-and-swap, atomic same-directory replace.
  Unsupported stores are never mutated, and secrets never reach
  diagnostics, cache names, or persisted metadata.

## [0.1.0] - 2026-08-11

### Added

- Initial release: `init`, `providers`, `usage`, `balance`, `quota`, and
  `tui` (alias `watch`) commands.
- Usage, cost, quota, and balance reporting for Anthropic, OpenAI, DeepSeek,
  Kimi (Moonshot), GLM (Z.ai/Zhipu), and Gemini, covering pay-as-you-go APIs
  and subscription plans.
- Usage aggregation with `--since`, `--until`, `--by`, `--provider`,
  `--source`, and `--group-by` filters, plus `--json` machine-readable output.
- Live auto-refreshing dashboard with token, cost, quota, and balance views,
  and colorized terminal output honoring `NO_COLOR` and `CLICOLOR_FORCE`.
- Release automation: Release Please maintains a release pull request;
  merging it finalizes this changelog and publishes five platform archives
  with SHA-256 checksums as GitHub Release assets.
