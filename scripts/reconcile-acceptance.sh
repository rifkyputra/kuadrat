#!/usr/bin/env bash
# kuadrat G5 reconcile acceptance. Needs root AND the sqlite3 CLI:
#   sudo bash scripts/reconcile-acceptance.sh
# Build first (as your user): PATH=$HOME/.cargo/bin:$PATH cargo build --release

set -uo pipefail

BIN=/home/kyy/devbox/kuadrat/target/release/kuadrat
DB=/var/lib/kuadrat/kuadrat.db
APP=g5demo
UNIT=kuadrat-${APP}
WORK=$(mktemp -d)

pass=0; fail=0
ok()  { echo "  PASS  $1"; pass=$((pass+1)); }
bad() { echo "  FAIL  $1"; fail=$((fail+1)); }
cleanup() { "$BIN" remove "$APP" >/dev/null 2>&1; rm -rf "$WORK"; systemctl daemon-reload; }
trap cleanup EXIT

[ -x "$BIN" ] || { echo "FATAL: $BIN not found — build it as your user first"; exit 1; }
[ "$(id -u)" -eq 0 ] || { echo "FATAL: run as root (sudo)"; exit 1; }
command -v sqlite3 >/dev/null || { echo "FATAL: this acceptance needs the sqlite3 CLI (apt install sqlite3)"; exit 1; }

echo "kuadrat G5 reconcile acceptance"

# A working fixture app.
mkdir -p "$WORK/$APP"
cat > "$WORK/$APP/Containerfile" <<'EOF'
FROM docker.io/library/alpine:3
CMD ["sh", "-c", "echo up; sleep 3600"]
EOF
cat > "$WORK/$APP/kuadrat.json" <<'EOF'
{"name":"g5demo","image":"","command":null,"env":[],"ports":[],"volumes":[],
 "secrets":[],"memory_max":"128M","health_cmd":null,"restart_policy":"Always","route":null}
EOF
git -C "$WORK/$APP" init -q
git -C "$WORK/$APP" -c user.email=t@t -c user.name=t add -A
git -C "$WORK/$APP" -c user.email=t@t -c user.name=t commit -qm v1

echo "== deploy g5demo"
"$BIN" deploy "$APP" "$WORK/$APP" 2>&1 | grep -q 'Done' && ok "deploy -> Done" || bad "deploy did not reach Done"

echo "== simulate a crash: inject an in_progress deploy + a held lock"
sqlite3 "$DB" "INSERT INTO deploys (app, stage, status) VALUES ('$APP', 'apply', 'in_progress');"
sqlite3 "$DB" "INSERT INTO locks (app, deploy_id) VALUES ('$APP', (SELECT max(id) FROM deploys));"
[ "$(sqlite3 "$DB" "SELECT count(*) FROM deploys WHERE status='in_progress';")" = "1" ] \
  && ok "injected a stuck in_progress deploy" || bad "injection failed"

echo "== the stuck lock blocks a new deploy"
# Capture before grepping: the blocked deploy exits non-zero BY DESIGN (only
# `Done` is success), and `set -o pipefail` would turn that rc=1 into the
# pipeline's status even when grep matched — a false FAIL. A plain assignment
# keeps the two apart: the rc we act on is grep's.
DEP_OUT=$("$BIN" deploy "$APP" "$WORK/$APP" 2>&1)
echo "$DEP_OUT" | grep -q 'already in progress' \
  && ok "the held lock blocks a deploy" || bad "a deploy was NOT blocked by the stuck lock"

echo "== reconcile"
"$BIN" reconcile 2>&1 | grep -qE 'RolledBack|Failed' && ok "reconcile reported a recovery" || bad "reconcile recovered nothing"
[ "$(sqlite3 "$DB" "SELECT count(*) FROM deploys WHERE status='in_progress';")" = "0" ] \
  && ok "no in_progress deploys after reconcile" || bad "an in_progress deploy remained"

echo "== the app is unblocked and still running"
"$BIN" deploy "$APP" "$WORK/$APP" 2>&1 | grep -q 'Done' && ok "deploy works again after reconcile" || bad "deploy still blocked after reconcile"
systemctl is-active --quiet "$UNIT" && ok "g5demo still active" || bad "g5demo not active"

echo "== RESULT"
echo "  passed: $pass    failed: $fail"
[ $fail -eq 0 ] && echo "  G5 RECONCILE ACCEPTANCE: PASS" || echo "  G5 RECONCILE ACCEPTANCE: FAIL"
exit $fail
