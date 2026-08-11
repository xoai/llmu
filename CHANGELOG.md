# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
