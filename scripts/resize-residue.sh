#!/usr/bin/env bash
# tmux 를 "리플로하는 터미널"로 써서 #76(리사이즈 잔상)을 재현·계측한다.
#
# 이 버그가 지금까지 관측 불가였던 이유는 하나다: 이미 그려진 행을 터미널이 **다시 접는 것**에서
# 나오는데, 저장소의 화면 모델 둘 다 리플로가 없다. tmux 3.7 은 리플로하고(실측 확인),
# 리사이즈를 프로그램으로 걸 수 있으며(`resize-window`), 화면을 텍스트로 읽을 수 있다
# (`capture-pane`). 그 셋이 모이면 재현된다.
#
# 재현 조건 (전부 필요했다):
#   - `-v` 없이   → footer 1행이 대상 경로가 되고, 잔상이 그 경로의 반복으로 보인다
#   - 긴 경로     → 좁은 폭에서 접혀야 한다. 접히지 않으면 아무 일도 안 일어난다
#   - 위에 출력   → footer 가 화면 맨 위에 있으면 재그리기가 잔상을 덮어버린다
#   - FIFO 원본   → 디스크 없이 복사를 원하는 만큼 살려둔다
#
# 사용: resize-harness.sh <바이너리> [사이클 수]
set -euo pipefail

BINARY=${1:-cprog}
CYCLES=${2:-3}
SESSION=cprog-resize-$$
WIDE=100
NARROW=48

command -v "$BINARY" >/dev/null 2>&1 || [ -x "$BINARY" ] || { echo "바이너리 없음: $BINARY" >&2; exit 1; }
[ -x "$BINARY" ] && BINARY=$(readlink -f "$BINARY")

WORK=$(mktemp -d -t cprog-resize-XXXXXX)
FEEDER=
cleanup() {
  tmux kill-session -t "$SESSION" 2>/dev/null || true
  [ -n "$FEEDER" ] && kill "$FEEDER" 2>/dev/null || true
  rm -rf -- "$WORK"
}
trap cleanup EXIT

mkfifo "$WORK/src.fifo"
mkdir -p "$WORK/destination-directory"
NAME="a-long-enough-name-to-wrap-when-narrow.iso"
DST="$WORK/destination-directory/$NAME"

( exec 3>"$WORK/src.fifo"
  for _ in $(seq 1 600); do head -c 65536 /dev/zero >&3; sleep 0.05; done ) &
FEEDER=$!

tmux new-session -d -s "$SESSION" -x "$WIDE" -y 14 \
  "printf 'line one above\\nline two above\\n\$ cp big.iso ./temp/\\n'; \
   CPROG_SLOW_THRESHOLD_MS=1 CPROG_SAMPLE_INTERVAL_MS=20 CPROG_RENDER_TICK_MS=50 \
   '$BINARY' '$WORK/src.fifo' '$DST'; sleep 30"
sleep 1.2

echo "바이너리: $BINARY"
for c in $(seq 1 "$CYCLES"); do
  for w in "$NARROW" "$WIDE"; do
    tmux resize-window -t "$SESSION" -x "$w" -y 14 2>/dev/null || true
    sleep 0.6
  done
  hits=$(tmux capture-pane -p -t "$SESSION" | grep -c -- "$NAME" || true)
  echo "  사이클 $c 후 (폭 $WIDE): 경로가 화면에 $hits 번"
done

echo
echo "=== 최종 화면 ==="
tmux capture-pane -p -t "$SESSION" | grep -n . | cut -c1-78 | sed 's/^/  |/'
echo
hits=$(tmux capture-pane -p -t "$SESSION" | grep -c -- "$NAME" || true)
echo "판정: 경로 $hits 회  (살아 있는 footer 한 줄만이어야 정상)"
if [ "$hits" -gt 1 ]; then echo ">>> 잔상 재현됨 (#76)"; else echo ">>> 잔상 없음"; fi
