# WarMachine On-Prem Build (DoD / CUI/ITAR)

WarMachine ships a dedicated **on-prem build variant** for environments that
handle CUI/ITAR data. Developer laptops run a WarMachine binary that can only
talk to an internal LLM server. The lockdown is **compiled in** — it is not a
setting, flag, or environment variable, so it cannot be switched off without
rebuilding the binary.

## Threat model

- Prompts, code, and tool output sent to the LLM may contain CUI/ITAR data.
- The binary must never transmit that data outside the controlled
  environment: no public LLM APIs, no cloud telemetry, no session sharing, no
  remote extension installs from outside the enclave.
- Every model request must leave a tamper-evident audit trail (supports
  NIST 800-171 3.3.1 audit logging).

## How the lockdown works

The `onprem` cargo feature (in the `warmachine`, `goose-providers`, and
`goose-cli` crates) bakes the policy in at compile time:

| Control | Mechanism |
|---|---|
| Endpoint | `WARMACHINE_ONPREM_BASE_URL` env var **at build time**. The build fails if it is unset, so an on-prem binary can never ship without an endpoint. Runtime path inputs such as `OPENAI_BASE_PATH` are still read, but every request URL is re-validated against the compile-time allowlist before it is sent (see below), so an override cannot steer requests — or the bearer token — off the allowlist. |
| Network allowlist | `goose_providers::onprem::check_url_allowed()` gates `ApiClient` construction — every provider HTTP request funnels through it — **and** the fully assembled request URL is re-checked in `ApiClient::build_url` on every request, because WHATWG `Url::join` semantics let an absolute-URL path silently replace the host. Origins outside the allowlist are refused. Cross-origin redirects are never followed in on-prem builds (same-origin redirect policy), so a redirect cannot carry the Authorization header off the allowlist. Plain `http` is rejected for non-loopback hosts (CUI/ITAR must be encrypted in transit). |
| Extra origins | `WARMACHINE_ONPREM_EXTRA_HOSTS` (optional, comma-separated) allowlists additional origins, e.g. an internal observability collector. |
| Provider registry | Only the OpenAI-compatible provider is registered. Cloud providers, ACP CLIs, and custom/declarative providers are not registered at all. The OpenAI Live protocol module (which dials api.openai.com) is compiled out, so live voice is unavailable in on-prem builds. |
| Self-update | `onprem` implies the `disable-update` feature: `warmachine update` fails closed instead of pulling a binary from the public internet. |
| Plugin hooks | Plugin shell hooks (`sh -c` commands embedded in plugin manifests) are hard-disabled in on-prem builds: the hook runner fails closed without executing, the shell-spawning machinery is compiled out, and every suppressed hook is audit-logged (`hook_suppressed` event). Plugins still load — only their shell hooks are suppressed. |
| Headless approvals | Non-interactive runs fail closed instead of silently auto-allowing: `Approve`/`SmartApprove` modes error immediately (no terminal to prompt), and `Auto` — the default mode — also errors rather than granting every tool call. Run interactively, or set `WARMACHINE_MODE=chat` for a tool-free headless session. |
| Plaintext diagnostics | LLM wire logs (`logs/llm_request.*.jsonl`), the `history.txt` prompt-history file, and large-tool-response spill files are never written to disk in on-prem builds — the writers are compiled out (history is kept in memory only, so in-session recall still works). A once-per-process sweep removes stale spill files left behind by non-on-prem builds. |
| Model downloads | Hugging Face model search/resolve/download fails closed in on-prem builds — the network choke point refuses before dialing huggingface.co. Use a pre-seeded local model cache. |
| Audit log | Every model request appends a hash-chained entry, including auxiliary model calls (session naming, summaries, permission judgments, tool-call labels, app-content completion) — recorded with a purpose label only, never request content. Session start/end, extension installs/removals, session exports, every tool call (allowed/denied), permission grants, recipe executions, keychain failures, suppressed plugin hooks, and audit verifications are logged too (see below). |
| Telemetry export | OTLP exporter endpoints must pass the network allowlist — an unallowlisted endpoint refuses to initialize, so traces can't leave the enclave. Langfuse is disabled unless its URL is on the allowlist. (Product telemetry was already permanently disabled.) |
| Session sharing | Nostr session publishing is disabled (default relays are public). |
| Remote MCP servers | `StreamableHttp` extension URIs must be on the allowlist. Unix-socket transports are local IPC and exempt. If a server issues an OAuth challenge, every discovered authorization/token/registration URL must also be on the allowlist (fail-closed); OAuth redirects are never followed. |
| Voice dictation | Cloud STT endpoints are not allowlisted, so dictation fails closed. |
| Audit log | Every model request appends a hash-chained entry; session start/end, extension installs/removals, session exports, every tool call (allowed/denied), permission grants, recipe executions, keychain failures, and audit verifications are logged too (see below). |
| At-rest encryption | Session message payloads in `sessions.db` are sealed with AES-256-GCM (see below). |
| Sealed metadata | Session titles, working directory, recipe parameters, recipe definitions, and extension configs (which carry MCP API keys and bearer tokens) are sealed at rest like message payloads — they can carry CUI (project names, paths, ticket IDs, credentials). |
| Secret storage | The OS keychain is mandatory: `WARMACHINE_DISABLE_KEYRING` is ignored and an unreachable keychain fails closed. |
| Tool approvals | Every shell/web tool call requires explicit human approval (see below). |
| FIPS 140-3 | TLS uses the FIPS-validated AWS-LC module (cert #4816); the binary refuses to start if FIPS mode is not active (see below). |
| Session retention | Sessions untouched for `WARMACHINE_SESSION_RETENTION_DAYS` (default 90) are purged automatically; retention cannot be disabled (see below). |
| Egress sandbox test | CI runs an integration test proving the allowlist refuses public clouds, plaintext HTTP, and bypass attempts (see below). |
| SBOM | Every CI run generates a CycloneDX SBOM for the on-prem dependency tree (see below). |

## Server requirements

- An **OpenAI-compatible** chat-completions endpoint
  (`POST {base}/chat/completions`, or with `/v1` prefix — both are handled),
  reachable from developer laptops.
- **TLS** with a certificate the laptops trust: either a public CA cert or an
  internal CA installed in the OS trust store. `https` is required (loopback
  excepted for local testing).
- Optional: **mTLS** client certificates — WarMachine already supports
  `tls_client_cert` / `tls_client_key` provider TLS config.
- The server should enforce its own authentication (API key or mTLS); the
  on-prem build sends whatever `OPENAI_API_KEY` is configured, but the
  endpoint itself is fixed.

## Building the on-prem binary

```bash
WARMACHINE_ONPREM_BASE_URL="https://llm.internal.example/v1" \
  cargo build --release -p goose-cli --features onprem
# optional extra origins:
WARMACHINE_ONPREM_EXTRA_HOSTS="https://otel.internal.example:4318" \
  WARMACHINE_ONPREM_BASE_URL="https://llm.internal.example/v1" \
  cargo build --release -p goose-cli --features onprem
```

Notes:

- `WARMACHINE_ONPREM_BASE_URL` is read **when compiling** `goose-providers`,
  not at runtime. Changing the endpoint means rebuilding.
- The desktop app must bundle this CLI binary (it shells out to it); a
  desktop build that bundles the standard binary is not an on-prem build.
- Verify the lockdown: point the binary at a public endpoint
  (e.g. `OPENAI_BASE_URL=https://api.openai.com`) — requests must fail with
  an allowlist error, and the provider list must show only the on-prem
  OpenAI-compatible provider.

## Audit log

Location: `~/.config/warmachine/audit.log` (one JSON object per line).

Two entry shapes, both hash-chained:

- **Model requests** (as before): `ts`, `session_id`, `model`, `endpoint`,
  `request_chars`, `request_sha256` (SHA-256 of the serialized request
  payload), `prev_hash`, `entry_hash`, where

  ```
  entry_hash = sha256(prev_hash | ts | session_id | model | endpoint | request_sha256)
  ```

  Auxiliary (non-agent-loop) model calls — session naming, context
  summaries, permission judgments, tool-call labels, app-content
  completion — are logged the same way, except the payload is a short
  **purpose label** (e.g. `"summarize"`), never request content: prompts and
  responses can carry CUI, so they stay out of the audit log. The one-shot
  helper (`complete_one_shot`) logs these centrally; the handful of direct
  call sites log the same way. A logging failure is warned and never
  propagated — a request must not fail because the audit write failed.
- **Events**: `ts`, `event` (`session_start`, `session_end`,
  `extension_added`, `extension_removed`, `session_export`, `tool_call`,
  `permission_grant`, `recipe_execute`, `credential_set`, `credential_removed`,
  `keychain_failure`, `hook_suppressed`, `audit_verified`,
  `audit_cursor_tamper`, `sessions_purged`), `session_id`, `details` (small metadata — tool name
  and allow/deny decision for `tool_call`, tool name and grant type for
  `permission_grant`, recipe title and source for `recipe_execute`, secret
  key name for `credential_set` / `credential_removed`, hook event name and
  plugin for `hook_suppressed`; never
  tool arguments, results, recipe parameters, or secret values),
  `prev_hash`, `entry_hash`, where

  ```
  entry_hash = sha256(prev_hash | ts | event | session_id | details_json)
  ```

Tool calls are audited in **both** agent-loop paths (the legacy loop and the
state-machine loop): every invocation records the tool name, session id, and
whether it was allowed or denied. Approval decisions are the actions that
actually touch CUI, so they get their own trail.

The log file itself is created with `0600` permissions (config dir `0700`),
writes are serialized with an exclusive file lock, and the log rotates at
100 MB: the archive (`audit-<UTC timestamp>.log`) keeps the chain — the new
file's first entry chains from the archived file's last hash, and `verify`
replays archives in order. Every 100th entry's hash is also written to a
checkpoint file that `verify` cross-checks, so a rewritten chain is caught
even without the SIEM copy (the forwarded SIEM copy remains the primary
external anchor).

The chain makes tampering or deletion detectable: each `prev_hash` must match
the previous `entry_hash`. Request *content* lives in the session database
(`sessions.db`); the audit log proves the sequence without duplicating
content. Audit writes are best-effort — a logging failure warns but never
breaks a request.

Verify the chain locally:

```bash
warmachine audit verify
# audit log verified: 128 entries, hash chain intact
```

`verify` also replays rotated archives in chronological order and reports
(rather than fails on) non-monotonic timestamps, which can indicate clock
tampering or skew. Each `verify` run appends an `audit_verified` event to
the log itself.

**SIEM forwarding:** the client creates no new egress path — forward the
JSONL file with your existing log shipper (e.g. Filebeat, Splunk Universal
Forwarder, or `rsyslog` imfile) to your SIEM. Each line is a self-contained
JSON event; the `entry_hash`/`prev_hash` fields let the SIEM (or a later
`warmachine audit verify` run) detect gaps or tampering in transit.

## Session data

Conversation history stays in the local `sessions.db` on the laptop, but
message payloads are **encrypted at rest** in the on-prem build:

- Each `content_json` row is sealed with **AES-256-GCM** under a per-install
  256-bit data-encryption key (DEK). The sealed value is a self-describing
  JSON envelope (`{"enc":"aes-256-gcm","v":1,"nonce":..,"ct":..}`), so no
  schema migration was needed.
- The DEK is generated on first run and held in the **OS keychain** — it never
  touches disk. If the keychain is unreachable, session writes fail closed
  rather than writing plaintext.
- Databases written before sealing was introduced still read: legacy
  plaintext rows are parsed as-is and re-sealed on their next write, so
  plaintext ages out through normal use. GCM also integrity-protects each
  row: tampered rows fail to open instead of decrypting to garbage.
- Sealing covers more than message bodies: the LLM-generated **session
  title** (`sessions.name`), the **working directory**, **recipe
  parameters** (`user_recipe_values_json`), **recipe definitions**
  (`sessions.recipe_json`), and **extension configuration**
  (`sessions.extension_data` — MCP server env vars and HTTP headers, which
  carry API keys and bearer tokens), and **model configuration**
  (`sessions.model_config_json` — provider request params are an open-ended
  map) are sealed too — all six can carry CUI or credentials and all six used to rest in plaintext.
- Because payloads are sealed, keyword search (`session list --match`,
  chat-recall) decrypts candidates in memory and matches in Rust instead of
  in SQL. Results are identical; large histories are somewhat slower.

Combine with full-disk encryption on the laptops and your organization's
device policy for defense in depth.

## Keychain requirement

The on-prem build treats the OS keychain as mandatory infrastructure:

- `WARMACHINE_DISABLE_KEYRING` (env var or config) is **ignored**.
- API keys and the session-encryption DEK are stored only in the keychain.
- If the keychain daemon is unreachable, secret reads/writes **fail closed**
  with an error — the build will not silently fall back to the plaintext
  `secrets.yaml` file. On a fresh laptop image, make sure the Secret Service
  (Linux) / Keychain (macOS) / Credential Manager (Windows) is functional
  before first run.

## Tool approvals

On-prem builds are **default-deny** for tool execution: every web tool call
requires explicit human approval in the CLI before it runs. Pattern-based
egress detection is bypassable (obfuscation, novel exfil paths); approval
is not.

Every tool invocation — allowed or denied — is written to the audit log
(`tool_call` event with tool name, session id, and decision; arguments and
results never enter the log), in both the legacy and state-machine agent
loops, plus ACP app-tool dispatches and code-execution tool sub-calls.
Durable permission grants (`permission_grant` event) and recipe executions
(`recipe_execute` event, identity only) are logged as well.

The **shell tool is removed entirely** in on-prem builds — it is not
registered, so the model never sees it, and direct invocations (including
the `!` bang-shell path) are refused at compile time. Rationale: arbitrary
shell commands can open network connections outside the build's allowlist,
and command-text filtering is bypassable, so the capability is removed
rather than sandbox-gated. Run commands in your own terminal.

The **Code Mode** platform extension (`code_execution`, with the
`execute_bash` / `execute_typescript` tools) is compiled out of on-prem
builds entirely, for the same reason: it runs arbitrary commands with the
user's full network access and no egress gating, which would undercut the
shell removal above. The `code-mode` cargo feature is a no-op under
`onprem` — the extension is never registered, so the model never sees it,
and enabling it by name fails closed (`Unknown extension:
code_execution`). This cannot be re-enabled at runtime or with feature
flags.

Operational notes:

- Approvals are per tool call, in the interactive CLI. There is no
  pre-approval list or "allow always" escape hatch in the on-prem build:
  the prompt offers Allow / Deny / Cancel only, `warmachine configure`
  cannot set a standing grant, and a stored `always_allow` entry (e.g. a
  hand-edited config file) degrades to ask-before at read time. Grant
  attempts are audit-logged (`permission_grant`).
- Headless use (recipes, `warmachine run`, scheduled jobs) **fails closed**
  in on-prem builds: with no terminal to answer the prompt, `Approve` /
  `SmartApprove` modes return an error immediately, and `Auto` — the
  default mode — does the same instead of silently granting every tool
  call. If you run unattended workflows, route them through a supervised
  session or accept that tool calls are refused. A run that needs no tool
  calls can set `WARMACHINE_MODE=chat` for a tool-free headless session.
- A denied call is reported to the model as a tool error, not a crash — the
  session continues.

## CI

`.github/workflows/ci.yml` includes a `rust-check-onprem` job that compiles
(`cargo check --all-targets`) and lints (clippy, `-D warnings`) the on-prem
feature combination on every push, so the gated code cannot rot. The same job
also runs the egress sandbox tests (below).

## FIPS 140-3 validated cryptography

The `onprem` feature implies the `fips` feature, which enables rustls's `fips`
mode: the TLS crypto backend switches from stock aws-lc-rs to the
FIPS-validated AWS-LC module (FIPS 140-3 certificate #4816).

- At startup, the binary installs the FIPS provider as the process default
  (`warmachine::onprem::init_fips_crypto`) before any TLS config is created.
  reqwest — used by every provider HTTP client — picks up the process-default
  provider, so all LLM traffic uses FIPS-approved algorithms.
- The binary **refuses to start** if the FIPS provider is not active
  (`is_fips_provider_active`), failing closed instead of silently running
  with non-validated cryptography.
- **Version pin.** FIPS 140-3 validation is version-specific: cert #4816
  covers specific AWS-LC builds. `aws-lc-fips-sys` is pinned (currently
  0.14.2) and CI fails if `Cargo.lock` drifts from the pin — a dependency
  bump must be a deliberate, reviewed decision, not a silent `cargo update`
  side effect. The `onprem_fips` integration test proves the provider
  actually activates at runtime (exercising the module's power-on
  self-tests), not just that the feature compiles.
- Scope note: FIPS validation covers the cryptographic module, not the whole
  binary. The certificate (#4816) covers specific AWS-LC versions and
  platforms — consult the
  [AWS-LC FIPS security policy](https://github.com/aws/aws-lc/blob/main/docs/FIPS.md)
  for the exact coverage. Running on a non-covered platform still gets
  FIPS-approved algorithms but is outside the validated boundary.

## Session retention

Sessions are purged automatically once they go untouched longer than the
retention period:

- `WARMACHINE_SESSION_RETENTION_DAYS` (runtime env var, default **90**).
- Enforcement runs on **every session start** (compiled in, not a cron job),
  plus on demand via `warmachine session purge-expired`.
- Retention **cannot be disabled**: `0` or an unparsable value fails closed
  rather than keeping sessions forever.
- Purged sessions are deleted with their messages and usage-ledger rows;
  each purge batch is recorded in the audit log (`sessions_purged` event).
- After a purge batch deletes rows, the database is `VACUUM`ed so purged
  CUI is actually reclaimed from SQLite free pages instead of lingering
  recoverably on disk.

## Egress sandbox test

`crates/goose-providers/tests/onprem_egress_sandbox.rs` (compiled only with
`--features onprem`) proves the network sandbox holds:

- Public LLM endpoints (OpenAI, Anthropic, Google, Cohere, OpenRouter) are refused.
- Plaintext HTTP to non-loopback hosts is refused.
- Allowlist bypass attempts (wrong port, lookalike hosts, subdomains,
  userinfo smuggling, non-HTTP schemes) are refused.
- `ApiClient::new` — the enforcement point every provider funnels through —
  fails for non-allowlisted hosts and succeeds for the primary endpoint.

CI runs this plus the `onprem` unit tests in the `rust-check-onprem` job.

## SBOM

Every CI run generates a [CycloneDX](https://cyclonedx.org/) SBOM for the
exact on-prem dependency tree (`onprem-sbom` job in `ci.yml`):

- `cargo cyclonedx` runs with the on-prem feature set for both `warmachine`
  and `goose-cli`.
- The resulting `*.cdx.json` files are uploaded as the
  `onprem-sbom-cyclonedx` artifact (90-day retention).

To generate locally:

```bash
cargo install cargo-cyclonedx
WARMACHINE_ONPREM_BASE_URL="https://llm.internal.example/v1" \
  cargo cyclonedx --format json -p warmachine --no-default-features \
  --features onprem,rustls-tls,code-mode,tree-sitter,live-voice,scheduler,platform-apps,chat-recall,acp-http,nostr,otel
```

## Vulnerability scanning (cargo-deny)

NIST 800-171 3.14.1 (flaw remediation) needs a real process for finding
vulnerable dependencies. CI runs `cargo deny check` on every push
(`cargo-deny` job in `ci.yml`), configured by `deny.toml`:

- **Advisories**: denies known vulnerabilities (RUSTSEC) and yanked crates.
- One ignore is documented in `deny.toml` (`RUSTSEC-2023-0071`, no safe
  upgrade available); every ignore carries a justification comment.

A failing advisory blocks the build — vulnerable dependencies cannot ship
silently.

## Audit log forwarding

The local hash-chained audit log is the source of truth, but a log that only
exists on the laptop dies with the laptop. On-prem builds can ship entries
to a SIEM:

- Bake the sink in at compile time:
  `WARMACHINE_ONPREM_AUDIT_SINK_URL="https://siem.internal.example/api/audit"`
  (plus optional `WARMACHINE_ONPREM_AUDIT_SINK_TOKEN` for a bearer token).
  Like the LLM base URL, the sink cannot be changed at runtime.
- At startup, a background task forwards new entries as NDJSON batches
  (`Content-Type: application/x-ndjson`, 500 entries/batch, 60s interval)
  over TLS — using the FIPS-validated provider, since forwarding starts
  after `init_fips_crypto()`.
- A cursor file (`audit.forward.cursor` next to `audit.log`, `0600`,
  HMAC-protected with a keychain-held key) records the last forwarded
  `entry_hash`, so restarts resume without duplicates. If the cursor's MAC
  fails verification, the forwarder logs loudly, writes an
  `audit_cursor_tamper` event into the audit log, and resumes from the
  pre-tamper tail so the tamper alert itself is forwarded. Non-JSON cursor
  content is treated as tamper too (full re-forward — safe duplication),
  so there is no unprotected legacy cursor format to downgrade to.
- At startup the baked-in sink URL is validated against the network
  allowlist and must use `https`; on mismatch the forwarder refuses to
  start (loud error, local log unaffected).
- Forwarding is **best-effort**: sink failures are logged and retried; the
  local log is unaffected and entries are never lost. If no sink URL was
  baked in, the forwarder is a complete no-op.
- First-run behavior: with no cursor, the forwarder ships the full history
  (batched). If the cursor goes stale (log truncated between passes, or the
  cursor file deleted), it anchors at the current end; backfilling after an
  anomaly is a manual operator task (`verify_audit_log` + any NDJSON
  shipper).
- Rotation handling: the forwarder replays rotated archives **chronologically
  before** the live log. If the audit log rotated between passes and the
  cursor is stranded in a rotated archive, the replay locates the cursor
  there and continues through newer archives into the live file — entries
  are no longer skipped after rotation. A rotation landing mid-pass can only
  duplicate entries, which is safe because the cursor is content-addressed
  by entry hash.

## Known limitations

Honest gaps and residual risks, kept current so operators plan around them:

- **Desktop prompt history.** The CLI keeps prompt history in memory only,
  but the **desktop app** has no compile-time `onprem` flag and still
  persists its own prompt history to disk. A desktop bundle is therefore
  not an on-prem build for this surface; the on-prem target is the CLI
  binary until the desktop carries the same gate.
- **External advisory not independently verified.** One dependency advisory
  (GHSA-6mg9-3cvh-9939, cited in an external review) could not be
  reproduced or independently confirmed: the advisory ID returned 404 from
  public sources, and it does not appear in the RustSec database. It is
  tracked as unverified, not as closed.
- **No on-device audit of SIEM reception.** The forwarder is best-effort;
  it does not confirm the SIEM received and persisted each batch. Treat
  the local hash-chained log as the source of truth and reconcile with the
  SIEM's copy operationally.
- **Unsigned commits in the development workflow.** WarMachine is pushed via
  the GitHub Git Database API with tree verification (byte-identical trees,
  checked on every push), but the resulting commits are **unsigned**.
  GPG/SSH-signed commits remain a nice-to-have for this pipeline.
- **Auxiliary-call coverage is purpose-label only.** Suppressed-hook,
  permission-judge, and other auxiliary audit entries record what ran and
  whether it was allowed — they do not record prompts or outputs. If an
  incident needs full request content, it lives only in the sealed
  `sessions.db`, not the audit trail.
