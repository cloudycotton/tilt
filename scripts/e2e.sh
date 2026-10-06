#!/usr/bin/env bash
# End-to-end checks of tilt in the desktop container (brief sections 8 and 9). Builds the
# image, then for each run starts the desktop with docker compose, runs tilt-probe inside it
# (plus Playwright on the host in run A), and prints a summary table. Exits 1 if any check
# failed. Probe reports, Playwright results and container logs land in e2e/results/.
#
#   A  1920x1080, xfce, static pattern               probe latency, click, idle frames and CPU;
#                                                    Playwright (Chrome, WebKit)
#   B  1024x768, X_ABSTRACT_ONLY=1, xfce, animated   E2B's setup: probe fps, latency, click
#   C  1920x1080, no desktop, animated               probe fps at 1080p
#
# Environment:
#   RUNS="A B C"  SKIP_BUILD=1  SKIP_PLAYWRIGHT=1
#   Targets (brief 9): PROBE_P50_MS=25 PROBE_P95_MS=45 IDLE_MAX=0 IDLE_CPU_MAX=2 FPS_1024=55
#                      FPS_1080=50 CHROME_P50_MS=50 CHROME_P95_MS=100 WEBKIT_P95_MS=150
set -euo pipefail
cd "$(dirname "$0")/.."

RUNS=${RUNS:-A B C}
PROBE_P50_MS=${PROBE_P50_MS:-25}
PROBE_P95_MS=${PROBE_P95_MS:-45}
IDLE_MAX=${IDLE_MAX:-0}
IDLE_CPU_MAX=${IDLE_CPU_MAX:-2}
FPS_1024=${FPS_1024:-55}
FPS_1080=${FPS_1080:-50}
CHROME_P50_MS=${CHROME_P50_MS:-50}
CHROME_P95_MS=${CHROME_P95_MS:-100}
WEBKIT_P95_MS=${WEBKIT_P95_MS:-150}

needs=(docker jq curl)
[ "${SKIP_PLAYWRIGHT:-0}" = 1 ] || needs+=(npm npx)
for tool in "${needs[@]}"; do
  command -v "$tool" >/dev/null || { echo "e2e.sh needs $tool on the PATH" >&2; exit 2; }
done

results=e2e/results
mkdir -p "$results"
rows=()
failed=0

# record RUN CHECK VALUE TARGET OK, where OK is 1 (pass), 0 (fail) or - (informational).
record() {
  local status
  case "$5" in
    1) status=PASS ;;
    -) status=info ;;
    *) status=FAIL; failed=1 ;;
  esac
  rows+=("$1|$2|$3|$4|$status")
}

# check RUN CHECK FILE VALUE-JQ TARGET OK-JQ: one summary row from a JSON report.
check() {
  local value ok
  value=$(jq -r "$4" "$3" 2>/dev/null) || value='?'
  if [ "$6" = - ]; then
    ok=-
  else
    ok=$(jq -r "if ($6) then 1 else 0 end" "$3" 2>/dev/null) || ok=0
  fi
  record "$1" "$2" "$value" "$5" "$ok"
}

# Leave no desktop running, however the script ends.
trap 'docker compose down --timeout 5 >/dev/null 2>&1 || true' EXIT

# up RUN SCREEN X_ABSTRACT_ONLY DESKTOP TESTPATTERN: a fresh desktop, healthy and reachable.
up() {
  local run=$1
  export SCREEN=$2 X_ABSTRACT_ONLY=$3 DESKTOP=$4 TESTPATTERN=$5
  echo "== run $run: SCREEN=$SCREEN X_ABSTRACT_ONLY=$X_ABSTRACT_ONLY DESKTOP=$DESKTOP TESTPATTERN=$TESTPATTERN"
  if docker compose up --detach --force-recreate --wait --wait-timeout 120 desktop \
    && curl -fsS --retry 10 --retry-delay 1 --retry-all-errors http://localhost:6090/healthz >/dev/null; then
    record "$run" "desktop up, /healthz" ok ok 1
    return 0
  fi
  record "$run" "desktop up, /healthz" failed ok 0
  down "$run"
  return 1
}

down() {
  docker compose logs --no-color desktop >"$results/desktop-$1.log" 2>&1 || true
  docker compose down --timeout 5 >/dev/null 2>&1 || true
}

# probe RUN OUT ARGS...: tilt-probe inside the container (TILT_TOKEN is set there). Records a
# row and returns non-zero if the probe could not produce a report.
probe() {
  local run=$1 out=$2 rc=0
  shift 2
  docker compose exec -T desktop tilt-probe --json "$@" >"$out" || rc=$?
  if [ "$rc" = 0 ] || [ "$rc" = 3 ]; then
    record "$run" "probe ran ($(basename "$out"))" "exit $rc" "exit 0 or 3" 1
    return 0
  fi
  record "$run" "probe ran ($(basename "$out"))" "exit $rc: $(jq -r '.error // "no report"' "$out" 2>/dev/null)" "exit 0 or 3" 0
  return 1
}

# probe_checks RUN FILE: what every probe report must show.
probe_checks() {
  check "$1" "decode errors" "$2" .decode_errors 0 ".decode_errors == 0"
  check "$1" "malformed server messages" "$2" .protocol_errors 0 ".protocol_errors == 0"
}

# The pattern logs the click that tilt-probe sends to pixel (100,100).
click_logged() {
  local ok=0
  if docker compose exec -T desktop \
    grep -q '"ev":"button_press","button":1,"x":100,"y":100}' /tmp/testpattern.log; then
    ok=1
  fi
  record "$1" "testpattern logged click at 100,100" "$([ $ok = 1 ] && echo yes || echo no)" yes "$ok"
}

# Brief 9: tilt under 2% CPU with one idle viewer. A probe that sends no input holds a session
# while tilt's CPU time is sampled over 3 s of the probe's idle phase, which starts once the
# refinement tail has been quiet for 1 s.
idle_cpu() {
  local out=$results/probe-idle-viewer.json pct probe_pid
  docker compose exec -T desktop \
    tilt-probe --json --duration-s 0 --latency-trials 0 --no-click --idle-s 8 >"$out" &
  probe_pid=$!
  sleep 4
  pct=$(docker compose exec -T desktop sh -c '
    pid=$(pgrep -xo tilt) || exit 1
    ticks() { sed "s/.*) //" "/proc/$pid/stat" | awk "{print \$12 + \$13}"; }
    a=$(ticks); sleep 3; b=$(ticks)
    awk -v a="$a" -v b="$b" -v hz="$(getconf CLK_TCK)" "BEGIN { printf \"%.2f\", (b - a) * 100 / (hz * 3) }"
  ') || pct='?'
  wait "$probe_pid" || true
  # Only a stream that really was idle makes this an idle measurement.
  local ok=0 note=''
  jq -e '.idle.settled and .idle.frames == 0' "$out" >/dev/null 2>&1 || note=' (not idle)'
  if [ "$pct" != '?' ] && [ -z "$note" ] && awk -v p="$pct" -v max="$IDLE_CPU_MAX" 'BEGIN { exit !(p < max) }'; then
    ok=1
  fi
  record A "tilt CPU %, 1 idle viewer, 3 s" "$pct$note" "< $IDLE_CPU_MAX" "$ok"
}

playwright() {
  if [ "${SKIP_PLAYWRIGHT:-0}" = 1 ]; then
    return 0
  fi
  if [ ! -d e2e/node_modules/@playwright/test ]; then
    (cd e2e && npm install --no-package-lock --no-audit --no-fund)
  fi
  rm -f "$results"/latency-*.json
  local rc=0
  (cd e2e && EXPECT_SCREEN=1920x1080 npx playwright test) || rc=$?
  record A "playwright (chrome, webkit)" "exit $rc" "exit 0" "$([ $rc = 0 ] && echo 1 || echo 0)"
  local f=$results/latency-chrome.json
  if [ -f "$f" ]; then
    check A "chrome key->drawn p50 (ms)" "$f" .p50 "<= $CHROME_P50_MS" ".n > 0 and .p50 <= $CHROME_P50_MS"
    check A "chrome key->drawn p95 (ms)" "$f" .p95 "<= $CHROME_P95_MS" ".n > 0 and .p95 <= $CHROME_P95_MS"
  fi
  f=$results/latency-webkit.json
  if [ -f "$f" ]; then
    check A "webkit key->drawn p50 (ms)" "$f" .p50 - -
    check A "webkit key->drawn p95 (ms)" "$f" .p95 "<= $WEBKIT_P95_MS" ".n > 0 and .p95 <= $WEBKIT_P95_MS"
  fi
}

run_a() {
  up A 1920x1080x24 0 xfce 1 || return 0
  local out=$results/probe-1080.json
  if probe A "$out" --duration-s 0 --latency-trials 30 --idle-s 3; then
    check A "probe key->decoded p50 (ms)" "$out" .latency_ms.p50 "<= $PROBE_P50_MS" ".latency_ms.n > 0 and .latency_ms.p50 <= $PROBE_P50_MS"
    check A "probe key->decoded p95 (ms)" "$out" .latency_ms.p95 "<= $PROBE_P95_MS" ".latency_ms.n > 0 and .latency_ms.p95 <= $PROBE_P95_MS"
    check A "probe trials without change" "$out" .latency_ms.failed 0 ".latency_ms.failed == 0"
    check A "probe click advanced marker" "$out" .click.ok true ".click.ok == true"
    check A "idle access units in 3 s" "$out" .idle.frames "<= $IDLE_MAX" "(.idle.frames | type) == \"number\" and .idle.frames <= $IDLE_MAX"
    probe_checks A "$out"
  fi
  click_logged A
  idle_cpu
  playwright
  down A
}

run_b() {
  up B 1024x768x24 1 xfce animate || return 0
  local out=$results/probe-1024-abstract.json
  if probe B "$out" --duration-s 5 --latency-trials 10; then
    check B "probe fps (animated)" "$out" .fps ">= $FPS_1024" ".fps >= $FPS_1024"
    check B "frame gap p95 (ms)" "$out" .gaps_ms.p95 - -
    check B "probe key->decoded p95 (ms)" "$out" .latency_ms.p95 - -
    check B "probe trials without change" "$out" .latency_ms.failed 0 ".latency_ms.failed == 0"
    check B "probe click advanced marker" "$out" .click.ok true ".click.ok == true"
    probe_checks B "$out"
  fi
  click_logged B
  down B
}

run_c() {
  up C 1920x1080x24 0 none animate || return 0
  local out=$results/probe-1080-animate.json
  if probe C "$out" --duration-s 5 --latency-trials 10 --no-click; then
    check C "probe fps (animated)" "$out" .fps ">= $FPS_1080" ".fps >= $FPS_1080"
    check C "frame gap p95 (ms)" "$out" .gaps_ms.p95 - -
    check C "probe key->decoded p95 (ms)" "$out" .latency_ms.p95 - -
    probe_checks C "$out"
  fi
  down C
}

if [ "${SKIP_BUILD:-0}" != 1 ]; then
  docker compose build desktop
fi
for run in $RUNS; do
  case "$run" in
    A) run_a ;;
    B) run_b ;;
    C) run_c ;;
    *) echo "unknown run '$run' (A, B or C)" >&2; exit 2 ;;
  esac
done

{
  printf '\n%-3s  %-38s  %-26s  %-12s  %s\n' RUN CHECK VALUE TARGET RESULT
  for row in "${rows[@]}"; do
    IFS='|' read -r run name value target status <<<"$row"
    printf '%-3s  %-38s  %-26s  %-12s  %s\n' "$run" "$name" "$value" "$target" "$status"
  done
} | tee "$results/summary.txt"
if [ "$failed" = 0 ]; then
  echo "e2e: all checks passed"
else
  echo "e2e: FAILED (details in $results/)"
fi
exit "$failed"
