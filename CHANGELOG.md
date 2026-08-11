# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
