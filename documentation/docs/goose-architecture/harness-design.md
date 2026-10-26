---
title: Harness (Principal Layer) Design
sidebar_position: 5
---

# Harness (Principal Layer) Design

Phase 0 design note for the WarMachine harness project. The governing
acceptance criteria (AC-1 through AC-7) and the phased plan live with the
project goal; this note records the architecture decisions those criteria
are implemented against, and the rationale for each.

## Goal

Add a principal layer on top of the existing agent engine so WarMachine can
complete work on the user's behalf, unattended: persistent memory, durable
tasks, a proactive daemon with triggers, asynchronous approvals with a full
audit trail, managed connectors, and delegation to worker sessions. The
harness ships in the personal profile only. The `onprem` variant is
unchanged by this project until a separately approved phase ports selected
components in internal-only form.

## Decisions

### D1 — Storage: SQLite via sqlx, one dedicated database (approved by Devin, 2026-10-25)

Memory records (Phase 1) and the task ledger (Phase 2) live in a dedicated
SQLite database, `harness.db`, in the WarMachine config directory,
managed with sqlx — the same stack the session store already uses.

- A dedicated database (not new tables in the session database) keeps
  upstream session-store migrations from colliding with harness schema,
  and lets the harness crate own its migrations end to end.
- WAL journal mode is required: it is what makes the store survive a
  `kill -9` mid-write (AC-1.5) and lets the CLI inspect state while the
  daemon holds the database open (AC-3.5).
- Rejected: a JSON file store, as the scheduler uses for jobs. Adequate
  for a small job list; inadequate for append-only event logs,
  supersession queries, and concurrent daemon/CLI access.

### D2 — Code placement: new `goose-harness` crate

Harness logic lives in a new workspace crate, `crates/goose-harness`,
depending on the core crate only where a hook is unavoidable. Session-start
brief injection, tool-path enforcement, and scheduler integration are the
expected touch points in `crates/goose`; each lands `cfg`-gated behind the
`harness` feature and as small as possible.

Rationale: upstream (aaif-goose) syncs stay cheap when new capability is a
new crate rather than edits spread across upstream-touched files, and the
feature boundary is what makes AC-7.3 mechanically checkable.

### D3 — Feature layout

- `goose-harness`: no features of its own in Phase 0.
- Core crate (`crates/goose`): `harness = ["dep:goose-harness"]`.
- CLI (`crates/goose-cli`): `harness = ["warmachine/harness"]`, and
  `harness` is in the CLI `default` set (Devin's decision, 2026-10-25:
  the harness is the product, not an opt-in extra). That makes the
  on-prem bundle the place where the boundary is enforced instead:
  `bundle-windows-onprem.yml` previously built the CLI with
  `--features onprem` and default features enabled, which would have
  compiled the harness into the on-prem binary once `harness` became a
  default. The bundle now builds with `--no-default-features` and an
  explicit feature list (the pre-harness default set plus `onprem`), so
  harness code never enters the locked-down binary (AC-7.3).
- The `onprem` feature sets in both crates are **not** modified. The
  on-prem CI job compiles an explicit feature list that does not include
  `harness`; AC-7.1/AC-7.3 are verified against that list each phase.

### D4 — Configuration: a `harness:` section in config.yaml

Harness configuration is a typed `harness:` section of the existing
`config.yaml`, deserialized into structs in `goose-harness::config`
(Phase 0 delivers the schema types and their defaults; consumers land with
their phases):

- `action_classes`: per-action-class lists of tool-name patterns, plus a
  default class. Defaults are conservative — `send`, `spend`, `delete`,
  and `shell` patterns are approval-required out of the box; nothing is
  auto-approved beyond read-only patterns the user lists explicitly.
  (Enforcement: Phase 4, AC-4.1.)
- `delivery`: quiet hours (local start/end), per-topic opt-outs, and a
  flag for whether proactive delivery is enabled at all.
  (Enforcement: Phase 3, AC-3.3.)
- `memory`: enabled flag and maximum brief size in tokens.
  (Consumer: Phase 1, AC-1.2.)
- `daemon`: event-trigger source selection. (Consumer: Phase 3.)

Unknown keys in the `harness:` section are rejected
(`deny_unknown_fields`) so a typo cannot silently weaken a policy.

### D5 — Audit log: extract, don't duplicate

The hash-chained audit log currently lives in `crates/goose/src/onprem.rs`
behind `#[cfg(feature = "onprem")]`. Phase 4 needs the same mechanism in
the personal profile (AC-4.4). Decision: when Phase 4 begins, extract the
chain implementation into a shared module usable by both profiles, with
on-prem behavior bit-for-bit unchanged and its existing tests as the
regression gate. Phase 0 changes nothing here; this note records the
decision so Phase 4 does not re-litigate it.

### D6 — Event trigger: loopback webhook first

Phase 3's event trigger is an HTTP webhook receiver bound to 127.0.0.1
only, authenticated with a per-install token stored in the OS keychain.
Rationale: it is the general mechanism (any local tool can POST), it adds
no non-loopback network surface, and a file-watcher trigger can be added
later as a second source without changing the trigger engine. Recorded
here; implemented in Phase 3.

## Reuse inventory

| Need | Existing piece | Verdict |
| --- | --- | --- |
| Scheduling | `crates/goose/src/scheduler/` + `schedule_tool` | Reuse as-is; daemon hosts it (Phase 3) |
| Transcript retrieval for consolidation | `chatrecall` platform extension | Reuse (Phase 1) |
| In-session todos | `todo` platform extension | Model reference only; durable ledger is new (Phase 2) |
| Tool approval plumbing | permission system + `tool_confirmation_coordinator` | Extend with a headless/async mode (Phase 4) |
| Hash-chained audit | `crates/goose/src/onprem.rs` | Extract and share (D5, Phase 4) |
| User channel | gateway + pairing (Telegram), non-onprem | Reuse for approvals (Phase 4) and workers (Phase 6) |
| Subagents | `subagent_execution_tool`, `orchestrator`, `summon` | In-session today; promoted to durable workers in Phase 6 |
| Secrets | OS keychain (onprem pattern) | Reuse for connector vault (Phase 5) |

## Non-decisions (deliberately deferred)

- Connector choice for Phase 5 (Gmail vs. Google Calendar) — decided at
  Phase 5 kickoff.
- Whether the desktop app gets task/worker UI in this project — the
  criteria require CLI visibility; desktop UI is a follow-up project.

## Phase 3 addendum — the daemon

`warmachine daemon run` is the long-lived harness process. It hosts
the existing recipe scheduler (cron jobs run while the daemon lives),
serves the loopback webhook trigger, dispatches trigger runs as
ledger tasks, and delivers completed task results through the
delivery policy.

### Operations

- `warmachine daemon run` — foreground; log to stdout.
- `warmachine daemon status` — pid, triggers, scheduler job count,
  and open/failed/pending-delivery task counts, from the heartbeat
  state file (`daemon-state.json` in the config dir; heartbeats every
  30 s — a state older than 90 s is reported as stale).
- `warmachine daemon stop` — SIGTERM to the recorded pid.
- `warmachine daemon unit` — prints the systemd user unit below.

### systemd user unit

```ini
[Unit]
Description=WarMachine harness daemon
After=network.target

[Service]
Type=simple
ExecStart=%h/.local/bin/warmachine daemon run
Restart=on-failure
RestartSec=5

[Install]
WantedBy=default.target
```

Install: save as `~/.config/systemd/user/warmachine-daemon.service`,
then `systemctl --user daemon-reload && systemctl --user enable --now
warmachine-daemon`. `loginctl enable-linger $USER` keeps it running
across logout.

### Webhook trigger

Configured under `harness.daemon.triggers` in config.yaml:

```yaml
harness:
  daemon:
    triggers:
      - name: ci-watch
        recipe: /path/to/triage.yaml
        topic: ci
```

Fire it with:

```sh
curl -X POST http://127.0.0.1:8471/trigger \
  -H "X-Harness-Token: $(cat <config-dir>/daemon.token)" \
  -d '{"trigger": "ci-watch"}'
```

The listener is loopback-only (a non-loopback `webhook_bind` is
refused at startup). The token lives in an owner-only file
(`daemon.token`, mode 0600) in the config dir until the Phase 5
credential vault lands. Responses: 202 + the ledger task id when the
run is accepted, 409 when the same trigger is already running (a
trigger never runs twice concurrently), 401 on a bad token, 404 on an
unknown trigger. `GET /health` answers unauthenticated for local
monitoring.

### Delivery policy

Every proactive delivery passes `DeliveryPolicy` first: master
switch, quiet hours (local time, default 22:00–07:00), per-topic
opt-outs, and a 24 h dedupe window keyed per item. Every decision —
allow or deny — is appended to the `delivery_log` table in
`harness.db` with its reason. Denied task results stay in the ledger
as `done` and are retried on the next 30 s tick; nothing is dropped
silently, and the atomic claim in the ledger keeps delivery
exactly-once.

## Phase 4 addendum — approvals & audit

### Approval queue

Approval-required actions in background runs are queued in the
`approval_requests` table (harness.db): action, class, origin, full
arguments, expiry. The requesting run polls the queue; the user
answers from any surface (`warmachine harness approvals
list|show|approve|deny`). Unanswered requests expire into denials —
default-deny on timeout — and a late decision cannot resurrect an
expired or decided request (decisions are guarded by a
`status = 'pending'` update).

### The gate

Enforcement sits in `ExtensionManager::dispatch_tool_call_inner`,
the single funnel both reply engines pass through before a tool
executes. Sessions whose type is `Scheduled` (every scheduler- or
daemon-dispatched run) are classified by tool name:

- forbidden → denied outright, audited;
- auto-approved → allowed, audited;
- approval-required → queued; the call waits for the decision.

Interactive sessions are never gated; they keep the in-band
permission flow. A session whose type cannot be resolved is treated
as interactive, so a lookup failure can never deadlock a live user
session behind the queue.

### Audit

`harness audit list|verify` reads a hash-chained NDJSON log in the
config dir. Entry format and chain math are byte-identical to the
on-prem build's audit component (same canonical string, same
GENESIS start), so a log from either build verifies under the
other's rules; format test vectors pin this.

One plan item narrowed on contact with the code: D5 proposed
extracting onprem's audit into goose-harness for shared use. The
on-prem build compiles the harness crate out by design (AC-7), so
it cannot link this module. The shared contract is the format —
implemented twice, pinned by vectors — not shared code. Rotation
and checkpoint files remain on-prem-only operational concerns.

### Critical-job alerts

Triggers marked `critical: true` alert on failure: primary channel
first (default `deliveries.md`, override
`harness.delivery.alert_primary_path`), fallback on failure
(default `alerts.md`, override `alert_fallback_path`). Attempts are
audit events: `alert_delivered`, `alert_primary_failed`,
`alert_fallback_used`.

## Phase 5 addendum — connectors & vault

Secrets live in the OS keychain (service `warmachine-harness`,
account = the secret's name), full stop. The harness vault never
falls back to plaintext file storage — where goose's provider
config degrades to a secrets file when the keychain is missing,
the vault errors instead, because a silent plaintext fallback
would void AC-5.1. The keychain cannot be enumerated, so a
metadata-only index in harness.db (account names and timestamps,
never values) backs `vault list`.

Connector definitions in config are references: transport
(command/args or URL) plus `env_secrets` mappings of environment
variable → vault account. There is no field a secret value could
be serialized into. Health is a real MCP `initialize` handshake
with the vault values injected as environment; for HTTP
connectors, an `AUTHORIZATION` env mapping is sent as the
Authorization header. Status therefore means "the server accepted
the credential", not "a process started".

The connect flow (`harness connectors connect <name>`) prompts
without echo for each missing secret, stores it, then runs the
handshake. OAuth-based services (Gmail) still need their
interactive consent in a browser — the flow covers token/API-key
connectors end-to-end; S6 (inbox drafting) goes live once a Gmail
connector is connected on the user's machine.
