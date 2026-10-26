#!/usr/bin/env bash
# harness-verify.sh — end-to-end verification of the WarMachine
# harness through the REAL binary. Every leg prints PASS/FAIL/SKIP;
# the script exits non-zero if any leg FAILs.
#
# Usage:
#   scripts/harness-verify.sh [--skip-build]
#
# Optional live-model leg (a real agent run behind the approval
# gate): set both variables to an OpenAI-compatible endpoint, e.g.
# a local Ollama:  HARNESS_VERIFY_MODEL_URL=http://localhost:11434/v1
#                  HARNESS_VERIFY_MODEL=llama3.2:3b
# Without them the leg reports SKIP — everything else still runs.
#
# On headless Linux the vault legs need a secret service; when
# dbus-run-session and gnome-keyring are available the script
# re-executes itself inside a session with an unlocked keyring.

set -u
cd "$(dirname "$0")/.."
REPO="$PWD"
# Repo toolchain (hermit) when available.
if [ -f bin/activate-hermit ]; then . bin/activate-hermit >/dev/null 2>&1 || true; fi
BIN="${WARMACHINE_BIN:-$REPO/target/debug/warmachine}"
FIXTURE="$REPO/crates/goose-harness/tests/fixtures/fake_mcp_server.py"

# --- keyring session wrap (headless Linux) -------------------------
if [ "$(uname)" = "Linux" ] && [ -z "${DBUS_SESSION_BUS_ADDRESS:-}" ] \
   && [ -z "${HARNESS_VERIFY_INNER:-}" ] && command -v dbus-run-session >/dev/null 2>&1; then
  export HARNESS_VERIFY_INNER=1
  exec dbus-run-session -- bash "$0" "$@"
fi
GKD=gnome-keyring-daemon
command -v "$GKD" >/dev/null 2>&1 || { [ -x /usr/bin/gnome-keyring-daemon ] && GKD=/usr/bin/gnome-keyring-daemon; }
if [ -n "${HARNESS_VERIFY_INNER:-}" ] && command -v "$GKD" >/dev/null 2>&1; then
  # Throwaway keyring storage for this run only — a fresh, unlocked
  # collection with a known password, never the user's real one.
  # (Kept OUTSIDE the run root: the script wipes the root mid-run.)
  export XDG_DATA_HOME="$REPO/.harness-verify-keyring"
  rm -rf "$XDG_DATA_HOME"; mkdir -p "$XDG_DATA_HOME"
  printf 'harness-verify' | "$GKD" --daemonize --unlock --components=secrets >/dev/null 2>&1 || true
  sleep 1
fi

PASS=0; FAIL=0; SKIP=0
ok()   { PASS=$((PASS+1)); echo "PASS  $1"; }
bad()  { FAIL=$((FAIL+1)); echo "FAIL  $1 — $2"; }
skip() { SKIP=$((SKIP+1)); echo "SKIP  $1 — $2"; }
expect_contains() { # name, haystack, needle
  if printf '%s' "$2" | grep -qF "$3"; then ok "$1"; else bad "$1" "expected output to contain: $3 (got: $(printf '%s' "$2" | head -c 200))"; fi
}
expect_exit() { # name, wanted(0|1), actual
  if [ "$2" = "$3" ]; then ok "$1"; else bad "$1" "expected exit $2, got $3"; fi
}

# --- build -----------------------------------------------------------
if [ "${1:-}" != "--skip-build" ]; then
  echo "== building warmachine (debug) =="
  if cargo build -p goose-cli --bin warmachine >/dev/null 2>&1; then ok "build"; else bad "build" "cargo build failed"; fi
fi
[ -x "$BIN" ] || { echo "FAIL  binary missing at $BIN"; exit 1; }

echo "== crate tests (goose-harness) =="
if cargo test -p goose-harness >/dev/null 2>&1; then ok "goose-harness tests"; else bad "goose-harness tests" "cargo test -p goose-harness failed"; fi

echo "== gate integration test (deterministic, no LLM) =="
if cargo test -p warmachine --features harness --test harness_gate >/dev/null 2>&1; then ok "approval gate (background sessions queue/block/decide)"; else bad "approval gate" "cargo test --test harness_gate failed"; fi

# --- isolated root ---------------------------------------------------
ROOT="$REPO/.harness-verify-run"
rm -rf "$ROOT"; mkdir -p "$ROOT/config"
export WARMACHINE_PATH_ROOT="$ROOT"
W() { "$BIN" harness "$@"; }
cleanup() { rm -rf "$ROOT" "$REPO/.harness-verify-keyring"; }
trap 'if [ $FAIL -eq 0 ]; then cleanup; else echo "kept failing run at $ROOT"; fi' EXIT

# --- config leg ------------------------------------------------------
cat > "$ROOT/config/config.yaml" <<EOF
harness:
  connectors:
    - name: fake-mcp
      command: "python3"
      args: ["$FIXTURE"]
      env_secrets:
        - env: DEMO_TOKEN
          account: demo-mcp-token
  daemon:
    triggers:
      - name: verify-check
        recipe: $ROOT/check.yaml
        topic: test
EOF
cat > "$ROOT/check.yaml" <<'EOF'
title: Verify Check
description: trivial trigger recipe
instructions: Reply with the single word "ok".
EOF
OUT=$(W config 2>&1); expect_contains "config parses" "$OUT" "fake-mcp"
cp "$ROOT/config/config.yaml" "$ROOT/good.yaml"
printf 'harness:\n  enabeld: true\n' > "$ROOT/config/config.yaml"
"$BIN" harness config >/dev/null 2>&1; expect_exit "invalid config key fails loudly" 1 $?
cp "$ROOT/good.yaml" "$ROOT/config/config.yaml"

# --- memory leg ------------------------------------------------------
OUT=$(W memory add --kind fact --subject "verify" "the sky is blue" 2>&1); expect_contains "memory add" "$OUT" "the sky is blue"
OUT=$(W memory brief 2>&1); expect_contains "memory brief round-trip" "$OUT" "the sky is blue"

# --- ledger leg ------------------------------------------------------
OUT=$(W tasks add "verify task" --owner harness-verify 2>&1)
TASK_ID=$(printf '%s' "$OUT" | grep -oE '[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}' | head -1)
[ -n "$TASK_ID" ] && ok "ledger add" || bad "ledger add" "no task id in: $OUT"
W tasks start "$TASK_ID" >/dev/null 2>&1
W tasks complete "$TASK_ID" "done-result" >/dev/null 2>&1
W tasks deliver >/dev/null 2>&1
W tasks deliver >/dev/null 2>&1
COUNT=$(grep -c "done-result" "$ROOT/config/deliveries.md" 2>/dev/null || echo 0)
[ "$COUNT" = "1" ] && ok "ledger exactly-once delivery" || bad "ledger exactly-once delivery" "deliveries.md has $COUNT copies"

# --- approvals + audit leg -------------------------------------------
OUT=$(W approvals request send_email --origin harness-verify --args '{}' 2>&1)
APPR_ID=$(printf '%s' "$OUT" | grep -oE '[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}' | head -1)
[ -n "$APPR_ID" ] && ok "approval queued" || bad "approval queued" "no id in: $OUT"
W approvals approve "$APPR_ID" >/dev/null 2>&1; expect_exit "approval decided" 0 $?
OUT=$(W approvals request spend_money --expiry-minutes 0 2>&1)
EXP_ID=$(printf '%s' "$OUT" | grep -oE '[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}' | head -1)
W approvals approve "$EXP_ID" >/dev/null 2>&1; expect_exit "expired approval refuses late decision" 1 $?
OUT=$(W audit verify 2>&1); expect_contains "audit chain verifies" "$OUT" "audit chain OK"
mkdir -p "$ROOT/tamper/config"; cp "$ROOT/config/audit.log" "$ROOT/tamper/config/audit.log"
python3 - "$ROOT/tamper/config/audit.log" <<'PYEOF'
import json, sys
p = sys.argv[1]
lines = open(p).read().splitlines()
e = json.loads(lines[0]); e["details"]["action"] = "forged"
lines[0] = json.dumps(e)
open(p, "w").write("\n".join(lines) + "\n")
PYEOF
OUT=$(WARMACHINE_PATH_ROOT="$ROOT/tamper" "$BIN" harness audit verify 2>&1); RC=$?
if [ $RC -ne 0 ] && printf '%s' "$OUT" | grep -q "entry 1"; then ok "tampered audit detected at entry 1"; else bad "tampered audit detected" "rc=$RC out=$OUT"; fi

# --- vault + connectors leg ------------------------------------------
# The keyring daemon can lag a beat behind session start; probe a
# few times before concluding there is no keychain.
OUT=""
for _ in 1 2 3 4 5; do
  OUT=$(W connectors list 2>&1)
  printf '%s' "$OUT" | grep -q "Error" || break
  sleep 2
done
if printf '%s' "$OUT" | grep -q "Error"; then
  skip "vault + connectors" "no OS keychain available: $(printf '%s' "$OUT" | head -c 120)"
else
  expect_contains "connector starts unconfigured" "$OUT" "missing vault secrets"
  OUT=$(printf 'wrong-token\n' | W connectors connect fake-mcp 2>&1)
  expect_contains "wrong token rejected by the server" "$OUT" "unauthorized"
  printf 'phase5-token-123\n' | W vault set demo-mcp-token >/dev/null 2>&1
  OUT=$(W connectors status --check 2>&1)
  expect_contains "right token healthy" "$OUT" "healthy=true"
  if grep -r "phase5-token-123" "$ROOT" >/dev/null 2>&1; then bad "secret hygiene" "token value found under the config root"; else ok "secret hygiene (token nowhere on disk)"; fi
  W connectors disconnect fake-mcp >/dev/null 2>&1
  OUT=$(W vault has demo-mcp-token 2>&1); expect_contains "disconnect removes the secret" "$OUT" "absent"
fi

# --- daemon leg --------------------------------------------------------
if curl -s --max-time 2 http://127.0.0.1:8471/health >/dev/null 2>&1; then
  bad "daemon leg" "port 8471 is already in use by another process — stop it first"
else
"$BIN" daemon run > "$ROOT/daemon.log" 2>&1 &
DAEMON_PID=$!
sleep 3
OUT=$(curl -s http://127.0.0.1:8471/health 2>&1); expect_contains "daemon health" "$OUT" "ok"
TOKEN=$(cat "$ROOT/config/daemon.token" 2>/dev/null)
OUT=$(curl -s -X POST http://127.0.0.1:8471/trigger -H "X-Harness-Token: $TOKEN" -d '{"trigger":"verify-check"}' 2>&1)
expect_contains "webhook trigger accepted" "$OUT" "accepted"
"$BIN" daemon stop >/dev/null 2>&1; sleep 2
if kill -0 $DAEMON_PID 2>/dev/null; then bad "daemon stops cleanly" "still running"; kill -9 $DAEMON_PID; else ok "daemon stops cleanly"; fi
fi

# --- live-model leg ----------------------------------------------------
# A real agent run through the daemon: the recipe must read a secret
# file with the shell tool (approval-required), so the run can only
# finish after the gate queues a request and this script approves it.
if [ -n "${HARNESS_VERIFY_MODEL_URL:-}" ] && [ -n "${HARNESS_VERIFY_MODEL:-}" ]; then
  echo "== live-model leg against $HARNESS_VERIFY_MODEL_URL ($HARNESS_VERIFY_MODEL) =="
  LIVE="$REPO/.harness-verify-live"
  rm -rf "$LIVE"; mkdir -p "$LIVE/config"
  TOKEN_WORD="verify-$(date +%s)"
  echo "$TOKEN_WORD" > "$LIVE/secret.txt"
  cat > "$LIVE/gated.yaml" <<EOF
version: "2"
title: Gated Read
description: live-leg recipe
extensions:
  - type: builtin
    name: developer
instructions: |
  Use the shell tool to run exactly this command: cat $LIVE/secret.txt
  Then reply with only the exact contents of that file and nothing else.
EOF
  cat > "$LIVE/config/config.yaml" <<EOF
harness:
  daemon:
    triggers:
      - name: gated-read
        recipe: $LIVE/gated.yaml
        topic: test
EOF
  OLLAMA_HOSTPORT=$(printf '%s' "$HARNESS_VERIFY_MODEL_URL" | sed -E 's#^https?://##; s#/.*$##')
  env WARMACHINE_PATH_ROOT="$LIVE" WARMACHINE_PROVIDER=ollama \
      WARMACHINE_MODEL="$HARNESS_VERIFY_MODEL" OLLAMA_HOST="$OLLAMA_HOSTPORT" \
      "$BIN" daemon run > "$LIVE/daemon.log" 2>&1 &
  LIVE_DAEMON=$!
  sleep 3
  DTOKEN=$(cat "$LIVE/config/daemon.token" 2>/dev/null)
  curl -s -X POST http://127.0.0.1:8471/trigger -H "X-Harness-Token: $DTOKEN" \
       -d '{"trigger":"gated-read"}' >/dev/null 2>&1
  APPR=""
  for i in $(seq 1 40); do
    sleep 15
    OUT=$(env WARMACHINE_PATH_ROOT="$LIVE" "$BIN" harness approvals list --status pending 2>/dev/null)
    if printf '%s' "$OUT" | grep -q pending; then
      APPR=$(printf '%s' "$OUT" | grep -oE '[0-9a-f]{8}-[0-9a-f-]{27,}' | head -1)
      break
    fi
  done
  if [ -z "$APPR" ]; then
    bad "live: gate queues the shell call" "no approval appeared within 600s (model may not have called the tool, or the provider failed)"
  else
    ok "live: gate queues the shell call"
    env WARMACHINE_PATH_ROOT="$LIVE" "$BIN" harness approvals approve "$APPR" >/dev/null 2>&1
    DONE=""
    for i in $(seq 1 40); do
      sleep 15
      OUT=$(env WARMACHINE_PATH_ROOT="$LIVE" "$BIN" harness tasks list 2>/dev/null)
      if printf '%s' "$OUT" | grep -q delivered; then DONE=1; break; fi
      if printf '%s' "$OUT" | grep -q failed; then break; fi
    done
    if [ -n "$DONE" ]; then ok "live: approved run completes and delivers"; else bad "live: approved run completes" "task did not reach delivered"; fi
    if python3 - "$LIVE/data/sessions/sessions.db" "$TOKEN_WORD" <<'PYEOF'
import sqlite3, sys
db = sqlite3.connect(sys.argv[1])
rows = db.execute("SELECT content_json FROM messages WHERE role='assistant'").fetchall()
sys.exit(0 if any(sys.argv[2] in r[0] for r in rows) else 1)
PYEOF
    then ok "live: model output contains the secret-file contents (tool really ran)"; else bad "live: tool output reached the model" "secret contents not found in any assistant message"; fi
  fi
  env WARMACHINE_PATH_ROOT="$LIVE" "$BIN" daemon stop >/dev/null 2>&1 || kill $LIVE_DAEMON 2>/dev/null
  rm -rf "$LIVE"
else
  skip "live model" "set HARNESS_VERIFY_MODEL_URL + HARNESS_VERIFY_MODEL (an Ollama endpoint) to run a real agent job behind the gate"
fi

echo
echo "harness-verify: $PASS passed, $FAIL failed, $SKIP skipped"
[ $FAIL -eq 0 ]
