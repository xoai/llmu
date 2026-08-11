# Provider integration notes

## Anthropic (implemented)
- Endpoints: `GET https://api.anthropic.com/v1/organizations/usage_report/messages`
  and `.../cost_report`. Requires an **Admin API key** (`sk-ant-admin...`),
  header `x-api-key` + `anthropic-version: 2023-06-01`.
- Usage supports `bucket_width=1m|1h|1d`, `group_by[]` (model, workspace,
  api_key, service_tier, ...), pagination via `has_more`/`next_page`.
- Token fields: `uncached_input_tokens`, `cache_read_input_tokens`,
  cache-creation tokens (flat or nested under `cache_creation`),
  `output_tokens`, plus `server_tool_usage` counts (e.g. web search).
- Claude Enterprise orgs use a different Analytics API (per-user attribution).
- Docs: https://platform.claude.com/docs/en/manage-claude/usage-cost-api

## Claude subscription / Claude Code (implemented, local)
- Transcripts: `~/.claude/projects/<project>/<session>.jsonl`; assistant
  messages carry `message.usage` with input/output/cache tokens and
  `message.model`. Dedupe on `(message.id, requestId)`.
- Plan limits come from the OAuth-authenticated usage endpoint read with
  local Claude credentials — wired as provider `claude` (see "Claude
  Pro/Max — live subscription meters" below).

## OpenAI (implemented)
- `GET https://api.openai.com/v1/organization/usage/completions` (also
  `/images`, `/audio`, `/embeddings`, ...) and `/v1/organization/costs`.
  Requires an **Admin key**, `Authorization: Bearer`.
- Usage buckets: `1m|1h|1d`; filters project_ids/user_ids/api_key_ids/models;
  fields `input_tokens` (total), `input_cached_tokens`, `output_tokens`,
  `num_model_requests`. Costs: `bucket_width=1d` only, `amount.value` USD.
- ChatGPT Plus/Pro subscription usage has no public API, but the Codex CLI
  path covers it — wired as provider `codex` (see "ChatGPT plan / Codex
  CLI" below).

## DeepSeek (implemented)
- `GET https://api.deepseek.com/user/balance` → `balance_infos[]` with
  `currency` (CNY|USD), `total_balance`, `granted_balance`,
  `topped_up_balance` as decimal strings. No usage-history endpoint, so llmu
  snapshots balances per run and derives spend from deltas.

## Kimi / Moonshot (implemented)
- `GET {base}/v1/users/me/balance` (base `api.moonshot.ai` or `.cn`) →
  `data.available_balance`, `voucher_balance`, `cash_balance`.
- Kimi Code membership quota (5h window + weekly) is served to the CLI via
  OAuth stored in `~/.kimi/credentials/`; undocumented. Reference:
  https://github.com/luisleineweber/usagebar (docs/providers/kimi.md).

## GLM — Z.ai / Zhipu (implemented)
- Coding-Plan quotas (5h token window, weekly, MCP monthly) come from
  monitor endpoints used by the subscription UI, **not** in the public API
  reference; auth is the raw API key in `Authorization` (no `Bearer`).
  Wired as provider `glm` — quotas and per-model usage (see "GLM Coding
  Plan" below).
- Endpoint shapes were recovered from:
  - https://github.com/robinebers/openusage (docs/providers/zai.md)
  - https://github.com/guyinwonder168/opencode-glm-quota
- Mind the two platforms: `api.z.ai` (global) vs `open.bigmodel.cn` (CN);
  keys are not interchangeable, and coding-plan traffic must use the
  `/api/coding/paas/v4` base or it bills the wallet instead of the plan.

## Gemini (implemented via client-side logging)
- Google routes Gemini API billing through Cloud Billing; programmatic
  history means BigQuery billing export or Cloud Monitoring — heavyweight.
- Lean path: every response includes `usageMetadata`
  (`promptTokenCount`, `candidatesTokenCount`, `cachedContentTokenCount`,
  `thoughtsTokenCount`); append one JSON line per request to a file and set
  `[gemini] usage_log`. llmu parses flat or nested shapes.
- Docs: https://ai.google.dev/gemini-api/docs/billing

---

## Endpoints recovered from CLIs & open-source trackers (2026-08)

Everything below was verified against the source code of the providers' own
CLIs and community usage trackers, not the official docs: it can change
without notice. Sources: `robinebers/openusage`, `steipete/CodexBar`,
`MoonshotAI/kimi-cli`, `guyinwonder168/opencode-glm-quota`, `openai/codex`,
`google-gemini/gemini-cli`.

### Claude Pro/Max — live subscription meters (wired: provider `claude`)

```
GET https://api.anthropic.com/api/oauth/usage
Authorization: Bearer <claudeAiOauth.accessToken>
anthropic-beta: oauth-2025-04-20
```

Token: `.credentials.json` in `$CLAUDE_CONFIG_DIR` / `~/.claude` /
`~/.config/claude` (macOS may use the keychain item "Claude Code-credentials";
export it and point `[claude].credentials` at the copy). Response:
`five_hour`, `seven_day`, `seven_day_sonnet` → `{utilization, resets_at}`,
plus `limits[]` entries of `kind: "weekly_scoped"` for model-scoped weekly
meters. Aggressively rate-limited — llmu calls it once per run. 401 = token
expired; running `claude` refreshes the file. Token refresh itself is
`POST https://platform.claude.com/v1/oauth/token` (`grant_type=refresh_token`).

### ChatGPT plan / Codex CLI (wired: provider `codex`)

Local, no network: `$CODEX_HOME/sessions/**/*.jsonl` (+ `archived_sessions/`).
`turn_context` lines carry the model; `event_msg` lines with
`payload.type == "token_count"` carry cumulative `info.total_token_usage`
(`input_tokens` includes `cached_input_tokens`), optional per-event
`last_token_usage`, and a `rate_limits` snapshot
(`primary` ≈ 5h, `secondary` ≈ weekly: `used_percent`, `window_minutes`,
`resets_in_seconds`). Caveat: subagent/forked sessions replay the parent's
token_count history — llmu's unchanged-cumulative skip removes most but not
all of it, so heavy subagent use can slightly overcount.

Live: `GET https://chatgpt.com/backend-api/wham/usage` with
`Authorization: Bearer <tokens.access_token>` and
`ChatGPT-Account-Id: <tokens.account_id>` from `$CODEX_HOME/auth.json`.
Returns `plan_type` and `rate_limit.{primary_window,secondary_window}`.
CONFIRMED live window schema (2026-08): `{used_percent,
limit_window_seconds, reset_after_seconds, reset_at}` with `reset_at` as
epoch SECONDS (session logs use `window_minutes`/`resets_in_seconds`
instead — llmu parses both). `secondary_window` may be null. Per-feature
meters appear under `additional_rate_limits[]` as `{limit_name,
rate_limit:{primary_window,…}}`. Reset credits live at
`.../wham/rate-limit-reset-credits`.

### GLM Coding Plan (wired: provider `glm`)

```
GET {base}/api/monitor/usage/quota/limit      # meters used by llmu
GET {base}/api/monitor/usage/model-usage      # per-model breakdown
    ?startTime=yyyy-MM-dd%20HH%3Amm%3Ass&endTime=…   # REQUIRED format
                                                     # (its 500 says so)
GET {base}/api/monitor/usage/tool-usage       # web-search/tool usage
GET {base}/api/biz/subscription/list          # plan name
Authorization: <api key>                      # RAW key — no "Bearer "
```

base = `https://api.z.ai` (global) or `https://open.bigmodel.cn` (CN) — set
`[glm].base_url`. Quota entries: `data.limits[]`. CONFIRMED live schema (Coding Max,
2026-08): `type: CREDIT_LIMIT` with absolute credits — `currentValue` used,
`usage` = total, `percentage`, `nextResetTime` epoch-ms, and the window in
`{unit, number}` (unit 3 = hours -> {3,5} is the 5h session; unit 6 = weeks
-> {6,1} is weekly). Other plan tiers may use `type: TOKENS_LIMIT`
(`percentage` meters) or `TIME_LIMIT` (`currentValue`/`usage` calls); llmu
parses all three. A 2xx `{"success":false, "msg":"...coding plan..."}`
means the key is valid but has no Coding Plan.

### Kimi For Coding (wired: provider `kimi`, `[kimi].code_key`)

```
GET https://api.kimi.com/coding/v1/usages
Authorization: Bearer <Kimi-Code key>
```

CONFIRMED live schema (2026-08): all numeric fields arrive as STRINGS
(`"limit":"100"`). `usage` = the weekly meter `{limit, used, remaining,
resetTime}` (RFC3339); `limits[]` entries repeat that shape under `detail`
with `window: {duration, timeUnit: TIME_UNIT_MINUTE|HOUR|DAY|WEEK}` —
e.g. `{300, TIME_UNIT_MINUTE}` is the 5h session. Plan tier is
`user.membership.level`. This is what kimi-cli's `/usage` screen renders.
Note the key is the *Kimi For Coding* credential, not the open-platform
`sk-` key.

### Gemini CLI / Code Assist (documented, not wired)

Gemini CLI's quota comes from the internal Code Assist backend:

```
POST https://cloudcode-pa.googleapis.com/v1internal:loadCodeAssist
POST https://cloudcode-pa.googleapis.com/v1internal:retrieveUserQuotaSummary
Authorization: Bearer <Google OAuth access token>
```

The token lives in `~/.gemini/oauth_creds.json` (written by `gemini` login)
but expires hourly, so a useful adapter must refresh it via
`https://oauth2.googleapis.com/token` with gemini-cli's public OAuth client
constants. Antigravity (Google's IDE) uses the same endpoints on
`daily-cloudcode-pa.googleapis.com`. Planned; contributions welcome.
