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

### D1 — Storage: SQLite via sqlx, one dedicated database

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
- CLI (`crates/goose-cli`): `harness = ["warmachine/harness"]` as an
  opt-in feature. It is deliberately **not** in the CLI `default` set:
  `bundle-windows-onprem.yml` builds the CLI with `--features onprem`
  and default features enabled, so anything in the CLI default set is
  compiled into the on-prem binary. (Upstream features such as
  `scheduler` already ride in that way; the harness does not get to.)
  Personal distribution builds enable `harness` explicitly, the same way
  the on-prem bundle enables `onprem`.
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
