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
- OpenCode local records map to `openai`; see the shared
  **OpenCode local usage** section below.

## DeepSeek (implemented)
- `GET https://api.deepseek.com/user/balance` → `balance_infos[]` with
  `currency` (CNY|USD), `total_balance`, `granted_balance`,
  `topped_up_balance` as decimal strings. No usage-history endpoint, so llmu
  snapshots balances per run and derives spend from deltas.
- OpenCode local records map to `deepseek`; see the shared
  **OpenCode local usage** section below.

## Kimi / Moonshot (implemented)
- `GET {base}/v1/users/me/balance` (base `api.moonshot.ai` or `.cn`) →
  `data.available_balance`, `voucher_balance`, `cash_balance`.
- Kimi Code membership quota (5h window + weekly) is served to the CLI via
  OAuth stored in `~/.kimi/credentials/`; undocumented. Reference:
  https://github.com/luisleineweber/usagebar (docs/providers/kimi.md).
- OpenCode local records map to `kimi`; see the shared
  **OpenCode local usage** section below.

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
- OpenCode local records map to `glm`; see the shared
  **OpenCode local usage** section below.

## Gemini (implemented via client-side logging)
- Google routes Gemini API billing through Cloud Billing; programmatic
  history means BigQuery billing export or Cloud Monitoring — heavyweight.
- Lean path: every response includes `usageMetadata`
  (`promptTokenCount`, `candidatesTokenCount`, `cachedContentTokenCount`,
  `thoughtsTokenCount`); append one JSON line per request to a file and set
  `[gemini] usage_log`. llmu parses flat or nested shapes.
- Docs: https://ai.google.dev/gemini-api/docs/billing

## Qwen / QwenCloud — Alibaba Cloud Model Studio (implemented, local)

Usage comes from **local Qwen Code records**, never from a Qwen network
call: llmu performs no network request to Qwen or QwenCloud, writes
nothing, and makes no built-in Qwen price guesses (per-model `[pricing]`
entries estimate cost when they match). Wired as provider `qwen`.

**Three non-interchangeable key classes.** Each class has its own llmu
config field, env/settings names, and base URL family; keys and base URLs
are **not interchangeable** between classes, and an `sk-sp-*` prefix
never identifies which plan class a key belongs to. A bare `sk-sp-*` value
under a standard variable stays standard; it is never promoted to either
plan.
- **Standard / pay-as-you-go** — `[qwen].standard_key`;
  `DASHSCOPE_API_KEY`, then `BAILIAN_API_KEY` (env and Qwen Code settings
  `env` block, same order). OpenAI-compatible hosts:
  - `https://dashscope.aliyuncs.com/compatible-mode/v1` (China Beijing)
  - `https://dashscope-intl.aliyuncs.com/compatible-mode/v1` (Singapore)
  - `https://dashscope-us.aliyuncs.com/compatible-mode/v1` (US Virginia)
  - `https://cn-hongkong.dashscope.aliyuncs.com/compatible-mode/v1` (Hong Kong)
- **Coding Plan** — `[qwen].coding_plan_key`;
  `BAILIAN_CODING_PLAN_API_KEY` only. OpenAI-compatible hosts:
  - `https://coding.dashscope.aliyuncs.com/v1` (China)
  - `https://coding-intl.dashscope.aliyuncs.com/v1` (international)
  - Official Alibaba Cloud docs also expose an Anthropic-compatible
    international endpoint:
    `https://coding-intl.dashscope.aliyuncs.com/apps/anthropic`.
- **Token Plan** — `[qwen].token_plan_key`;
  `BAILIAN_TOKEN_PLAN_API_KEY` only. OpenAI-compatible hosts:
  - `https://token-plan.cn-beijing.maas.aliyuncs.com/compatible-mode/v1`
  - `https://token-plan.ap-southeast-1.maas.aliyuncs.com/compatible-mode/v1`

**Paths and precedence.** Qwen Code settings live at
`${QWEN_HOME:-~/.qwen}/settings.json`; llmu reads the `env` block and
`advanced.runtimeOutputDir` from it read-only and records provenance in
`llmu providers`.

- Home: `[qwen].home` > `QWEN_HOME` > `~/.qwen`.
- Runtime: `[qwen].runtime_dir` > `QWEN_RUNTIME_DIR` >
  settings `advanced.runtimeOutputDir` > effective Qwen home. A relative
  `runtimeOutputDir` anchors under the effective Qwen home, never the
  process working directory.

**Local usage sources** (Qwen-family `qwen*` models only; GLM/Kimi/DeepSeek
rows recorded by Qwen Code are never relabeled):

- Request ledger: every `usage/token-usage-*.jsonl` under the effective
  runtime directory. Filenames use the writer's local month
  (`token-usage-YYYY-MM.jsonl`), so llmu reads all matching files and
  filters records by their RFC3339 UTC timestamp, deduplicating on the
  record id. `inputTokens` already includes cached tokens (uncached input =
  input − cached); `thoughtsTokens` folds into output.
- Legacy fallback: `${home}/usage_record.jsonl` session summaries, last
  valid record wins per session; a session covered by the request ledger
  suppresses its legacy summary entirely — no double counting. Sessions
  without request records emit one event per Qwen model.
- Routed Claude Code rows with `qwen*` models (served through an
  Anthropic-compatible route) are attributed to `qwen` too. The two
  clients' records represent disjoint requests and are additive — llmu
  performs no cross-client deduplication, and `--provider qwen` includes
  both.

**Trust boundary.** QwenCloud account analytics, free-tier/subscription
quota, and billing are console-only: there is no documented
account-reporting API authenticated by an inference or plan key. Browser
cookies and the console `sec_token` are intentionally never discovered,
stored, or used, and llmu never scrapes the console `/data/api.json`
endpoints. A key-only configuration shows as configured but claims no live
account usage; malformed or unreadable local records are skipped with one
aggregate secret-free note each.

OpenCode local records for Qwen aliases are read through the shared
**OpenCode local usage** section below.

## OpenCode local usage (implemented, local)

llmu reads completed assistant usage records from OpenCode's local SQLite
database, read-only, and reports them under llmu's canonical provider
identities — Qwen, GLM, DeepSeek, Kimi, and OpenAI (see each provider's
section above for its other sources). This is separate from OpenCode
`auth.json` credential discovery: auth supplies keys for some providers;
usage reads message records and never touches credentials.

**Paths and precedence.** One shared resolver supplies both the auth and
the database path: non-empty `OPENCODE_DATA_DIR`, else `XDG_DATA_HOME`
joined with `opencode`, else `~/.local/share/opencode`. The database is
`<effective-dir>/opencode.db`. A missing database is silent; usage simply
reports no OpenCode records.

**Read-only, WAL-aware access.** The live database is opened read-only
(no create, no URI interpretation) with `query_only` enabled and a 250 ms
busy timeout, so current `-wal`/`-shm` state stays visible; `immutable=1`
is never used and the database is never copied, snapshotted, written,
migrated, or vacuumed. Only the `message` table is queried
(`time_created`, `data`); `event` and `part` tables are never read. No
browser cookies, console endpoints, or network calls are involved, and no
message text, prompts, IDs, paths, SQL, or credentials ever appear in
diagnostics.

**Provider attribution.** Attribution uses an exact, case-sensitive
allowlist of OpenCode `providerID` values — never model-prefix inference:

| OpenCode providerID | llmu provider |
|---|---|
| `alibaba`, `alibaba-cn`, `alibaba-coding-plan`, `alibaba-coding-plan-cn`, `alibaba-token-plan`, `alibaba-token-plan-cn`, `bailian-token-plan-personal` | `qwen` |
| `zai`, `zai-coding-plan`, `zhipuai`, `zhipuai-coding-plan` | `glm` |
| `deepseek` | `deepseek` |
| `kimi-for-coding`, `moonshot`, `moonshotai`, `kimi` | `kimi` |
| `openai` | `openai` |

Unknown or generic (`opencode`) provider IDs are skipped and counted once;
no model id is inspected to guess a provider.

**Eligibility.** A record counts only as a completed assistant message:
`role` is `assistant`; `error` is absent or null; `finish` is exactly
`tool-calls`, `stop`, or `length`; `time.created` equals the database row
timestamp and `time.completed` is present; every token bucket is a
nonnegative integer; the normalized total is positive; and no decoded JSON
key is duplicated at any nesting depth. Records that fail any of these —
user/error/incomplete roles, malformed JSON, wrong types, negative or
fractional numbers, duplicate decoded keys — are skipped and counted once
as malformed.

**Token and cost semantics.** `tokens.input` is fresh input (never
cache-subtracted), `tokens.reasoning` folds into output with saturating
addition, `tokens.cache.read`/`write` map directly, and every aggregate
and derived total saturates at `u64::MAX`. Records are bucketed hourly and
aggregated per provider/model/hour. OpenCode's stored local `cost` is
ignored: the only cost shown is llmu's `[pricing]`-table estimate — never
a provider-billed or account amount.

**Overlap and filters.** OpenCode records are additive with provider APIs
and other client logs; a request run through OpenCode may also appear in an
API aggregate or another client's log. llmu cannot prove overlap without
stable shared request ids and never deduplicates across sources, so
overlapping feeds can overcount. Existing `--provider`, `--model`, and
`--source local` filters apply unchanged; `--provider qwen` admits every
mapped Qwen alias.

**Standalone status.** `llmu providers` shows a standalone `opencode` row
that is `yes` only when a read-only one-row query finds a record accepted
by the same strict validator and provider allowlist as collection;
missing, unreadable, busy, incompatible, or rejected-only databases answer
`no`. The probe is lighter than collection — it returns one constant and
performs no aggregation — but it is not constant-time: with no matching
record it may examine every `message` row. The status table prints
no diagnostics; detailed diagnostics appear only as `llmu usage` notes.

**Diagnostics.** `llmu usage` prints at most one bounded note per category
— malformed records, records from unsupported providers, a busy or unreadable
database, or an unsupported schema — and nothing else: no paths, SQL text,
record JSON, provider/model values, IDs, or credentials. A missing database
is silent.
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
meters. Aggressively rate-limited — llmu calls it once per run.

OAuth refresh (file-backed `claudeAiOauth` entries only, FR-6):
- Proactive refresh when the access token expires within five minutes
  (`expiresAt <= now + 300000ms`) via
  `POST https://platform.claude.com/v1/oauth/token` with
  `grant_type=refresh_token`, the stored refresh token, the stored
  `clientId` or the pinned public installed-app id
  `9d1c250a-e61b-44d9-88ed-5944d1962f5e` (a public upstream constant
  from Claude Code 2.1.227, not a user secret), and space-joined scopes.
  No client secret and no bearer header are sent.
- rotated `refresh_token` values are persisted back to the file
  atomically: unknown fields preserved, Unix mode `0600`, an llmu-only
  sibling lock, and a compare-and-swap immediately before replacement.
  Any HTTP, validation, or concurrency failure leaves the file unchanged.
- Exactly one reactive 401 retry: llmu re-reads the file, adopts a token
  another process installed, or forces one refresh and retries once.
  Usage 403/429/5xx never triggers a refresh or retry.
- A transient proactive refresh failure falls back to the still-valid
  access token with a warning. A refresh token expiring within three days
  emits a warning without blocking an otherwise valid quota request.
  Permanent failures (`invalid_grant`) preserve the file and tell the
  user to log in again with Claude Code.
- Direct access tokens (e.g. OpenCode's `auth.json` `anthropic` OAuth
  entry) are never refreshed: a usage 401 says to refresh Anthropic
  authentication in OpenCode or configure Claude Code credentials, and
  never claims llmu can refresh an access-only token.

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

### Gemini CLI / Code Assist (wired: provider `gemini`)

Gemini's Code Assist quota comes from the internal Code Assist backend:

```
POST https://cloudcode-pa.googleapis.com/v1internal:loadCodeAssist
POST https://cloudcode-pa.googleapis.com/v1internal:retrieveUserQuota
Authorization: Bearer <Google OAuth access token>
```

`retrieveUserQuota` is the corrected endpoint (Gemini CLI v0.39.1 and
current main). Migration note: the roadmap previously named the stale
endpoint `retrieveUserQuotaSummary`; llmu never calls it.

Credentials: Gemini CLI's plaintext `oauth_creds.json`
(`${GEMINI_CLI_HOME:-$HOME}/.gemini/oauth_creds.json`, override
`[gemini] credentials`). Tokens expire hourly, so llmu refreshes via
`https://oauth2.googleapis.com/token` (`grant_type=refresh_token`) using
Gemini CLI v0.39.1's public installed-app OAuth constants — client id
`681255809395-oo8ft2oprdrnp9e3aqf6av3hmdib135j.apps.googleusercontent.com`
and client secret `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` — which are
public upstream application identifiers, not user credentials. Refresh
persists rotated tokens atomically back to the file: unknown fields
preserved, Unix mode `0600`, llmu-only sibling lock plus compare-and-swap.
An unexpired access token remains usable without a refresh token; an
expired one without a refresh token says to run `gemini` again.

Encrypted/keychain storage is unsupported in this release: if
`oauth_creds.json` is absent but the sibling `gemini-credentials.json`
marker exists (Gemini CLI v0.39.1's keychain fallback), llmu reports the
limitation and never reads or mutates that store. Keychain-only storage
without the marker is not detectable. llmu never calls `onboardUser` and
never mutates account state: missing onboarding, ineligible tiers, and
missing projects produce remediation to run `gemini` or set `[gemini]
project` / `GOOGLE_CLOUD_PROJECT` (precedence: config >
`GOOGLE_CLOUD_PROJECT` > `GOOGLE_CLOUD_PROJECT_ID`; a project returned by
`loadCodeAssist` is authoritative). Malformed quota buckets are skipped
with diagnostics; if none remain, llmu reports payload drift. API
failures degrade through the last-known-good quota cache. Antigravity
(Google's IDE) uses the same endpoints on `daily-cloudcode-pa.googleapis.com`.
