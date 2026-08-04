#!/usr/bin/env bash
# 로그 릴레이 경로(erase -> 로그 -> redraw)를 최대한 세게 돌려 바 깜빡임을 보거나 세는 도구.
#
# 왜 이 조건이어야 하나 — 그냥 큰 파일 하나를 복사하면 `cp -v` 줄이 한 번만 나오고 릴레이
# 경로가 거의 안 돈다. footer가 계속 떠 있으면서 로그가 쏟아지는 상태를 만들어야 하고, 그게
# `CPROG_SLOW_THRESHOLD_MS=1`(모든 파일이 "느림"으로 판정) + 파일 여러 개다. 이 변수를 안 걸면
# footer가 아예 안 떠서 아무것도 관측되지 않는다 (#80).
#
#   ./scripts/flicker.sh                     현재 PATH의 cprog로 육안 시험
#   ./scripts/flicker.sh -b target/release/cprog
#   ./scripts/flicker.sh --strace            write() 경계를 세고 끝 (눈 안 씀, TTY 불요)
#   ./scripts/flicker.sh -n 60 -s 10M
#   ./scripts/flicker.sh -d ~/scratch          느린 디스크에 두면 육안으로 볼 만큼 길어진다
#                                              (tmpfs 는 너무 빨라 눈으로 못 본다)
#
# A/B는 바이너리를 바꿔 두 번 돌린다:
#   git switch <before> && cargo build --release && cp target/release/cprog /tmp/cprog-before
#   git switch <after>  && cargo build --release && cp target/release/cprog /tmp/cprog-after
#   ./scripts/flicker.sh -b /tmp/cprog-before
#   ./scripts/flicker.sh -b /tmp/cprog-after
#
# 터미널이 망가졌을 때: printf '\033[r'; tput cnorm

set -euo pipefail

COUNT=30
SIZE=10M
BINARY=cprog
MODE=visual
KEEP=0
PARENT=

usage() { sed -n '2,25p' "$0" | sed 's/^# \{0,1\}//'; exit "${1:-0}"; }

while [ $# -gt 0 ]; do
  case "$1" in
    -n|--count)  COUNT=$2; shift 2 ;;
    -d|--dir)    PARENT=$2; shift 2 ;;
    -s|--size)   SIZE=$2; shift 2 ;;
    -b|--binary) BINARY=$2; shift 2 ;;
    --strace)    MODE=strace; shift ;;
    --keep)      KEEP=1; shift ;;
    -h|--help)   usage 0 ;;
    *) echo "알 수 없는 인자: $1" >&2; usage 1 ;;
  esac
done

command -v "$BINARY" >/dev/null 2>&1 || [ -x "$BINARY" ] || {
  echo "cprog 바이너리를 못 찾았다: $BINARY" >&2
  echo "  cargo build --release 후 -b target/release/cprog 로 지정하거나," >&2
  echo "  cargo install --path . --locked 로 설치해라." >&2
  exit 1
}
[ -x "$BINARY" ] && BINARY=$(readlink -f "$BINARY")

if [ "$MODE" = strace ] && ! command -v strace >/dev/null 2>&1; then
  echo "--strace 모드에는 strace 가 필요하다." >&2; exit 1
fi
if [ "$MODE" = visual ] && [ ! -t 1 ]; then
  echo "stdout 이 터미널이 아니다 — cprog 는 passthrough 로 가고 바가 아예 안 뜬다." >&2
  echo "실제 터미널에서 직접 실행해라. (계측만 원하면 --strace)" >&2
  exit 1
fi

# 우리가 만든 디렉터리만 지운다.
# `-d` 는 픽스처를 놓을 곳을 고른다. 기본은 `mktemp` 의 기본값(보통 tmpfs)인데, tmpfs 는
# 램 속도라 복사가 눈 깜짝할 새 끝난다 — 육안으로 보려면 진짜 디스크 위의 경로를 줘라.
if [ -n "$PARENT" ]; then
  mkdir -p -- "$PARENT"
  WORK=$(mktemp -d -p "$PARENT" cprog-flicker-XXXXXX)
else
  WORK=$(mktemp -d -t cprog-flicker-XXXXXX)
fi
cleanup() {
  if [ "$KEEP" = 1 ]; then echo; echo "작업 디렉터리 유지: $WORK"
  else rm -rf -- "$WORK"; fi
}
trap cleanup EXIT

echo "픽스처 생성: ${COUNT} x ${SIZE}  (${WORK})"
mkdir -p "$WORK/src"
# 난수 블록 하나를 만들고 복제한다. /dev/urandom 을 파일마다 읽으면 느리다.
# 0 으로 채우면 안 된다 — `cp --sparse=auto` 가 구멍을 만들어 대상 크기가 뛰고,
# 진행 모델이 sparse 경로(exceptions E4/F18)로 들어가 이 시험의 관심사가 아니게 된다.
head -c "$SIZE" /dev/urandom > "$WORK/block"
for i in $(seq 1 "$COUNT"); do cat "$WORK/block" > "$WORK/src/f$i.bin"; done
rm -f "$WORK/block"
du -sh "$WORK/src" | sed 's/^/  /'

# 이 시험이 성립하는 조건. 셋 다 일시적이고 이 프로세스에만 걸린다.
export CPROG_SLOW_THRESHOLD_MS=1   # 모든 파일이 "느림" -> footer 가 계속 떠 있다
export CPROG_SAMPLE_INTERVAL_MS=10 # 파일 하나가 수십 ms 라 기본 100ms 면 샘플이 안 쌓인다
export CPROG_RENDER_TICK_MS=33

if [ "$MODE" = strace ]; then
  TRACE="$WORK/trace.txt"
  # script(1) 로 PTY 를 물려야 managed 로 들어간다. `-s 600` 이 없으면 strace 가 페이로드를
  # 32자에서 잘라 바(`%`)가 트레이스에 안 나타나고, 판정이 조용히 0을 낸다.
  # `stty` 로 PTY 크기를 명시한다. `script` 의 stdout 이 파이프면 크기가 0x0 이 되고,
  # 크기를 모르면 footer 를 아예 안 그리는 렌더러에서는 **아무것도 관측되지 않은 채 0 이 나온다**
  # — "틈 없음" 이 아니라 "안 쟀음" 이다. 실제로 그렇게 한 번 속았다.
  script -qec "stty rows 24 cols 100; \
               strace -s 600 -o '$TRACE' -e trace=write -ttt '$BINARY' -rv '$WORK/src' '$WORK/dst'" \
    /dev/null >/dev/null 2>&1 || true

  if ! grep -q '%' "$TRACE" 2>/dev/null; then
    echo "  !! 트레이스에 바가 없다 — footer 가 한 번도 안 떴다는 뜻이고, 아래 수치는 무의미하다." >&2
    echo "     managed 진입 조건(TTY 크기·TERM·stdbuf)을 확인해라." >&2
  fi

  echo
  echo "복사된 파일: $(find "$WORK/dst" -type f 2>/dev/null | wc -l) / $COUNT"
  echo
  TRACE="$TRACE" python3 - <<'PYEOF'
import os, re

W = re.compile(r'^(\d+\.\d+) write\(1, "(.*)", (\d+)\)\s*=')

# **렌더러에 독립적으로** 상태를 따라간다. 지우기 시퀀스를 패턴으로 찾으면 렌더러가 바뀔 때
# 조용히 0을 낸다 — 실제로 두 번 속았다(옛 렌더러는 `\r ESC[K`, DECSTBM 은 `ESC7 CUP ESC[J ESC8`).
# 대신 화면 상태만 본다: 바(`%`)를 담은 write 가 나가면 바가 떠 있고, 지우기(`ESC[K`/`ESC[J`)를
# 담았는데 바가 없으면 그 순간부터 화면에 바가 없다. 로그만 담긴 write 는 상태를 안 바꾼다.
ev = []
for line in open(os.environ["TRACE"]):
    m = W.match(line)
    if m:
        ev.append((float(m.group(1)), m.group(2)))
if not ev:
    raise SystemExit("  write 이벤트가 없다 — managed 로 안 들어갔을 수 있다")

spans, on, since, seen = [], False, None, False
for t, p in ev:
    if "%" in p:
        if not on and since is not None:
            spans.append((t - since, t))
        on, seen = True, True
        since = None
    elif ("33[K" in p or "33[J" in p) and on:
        on, since = False, t
elapsed = ev[-1][0] - ev[0][0]
if not seen:
    raise SystemExit("  바가 한 번도 안 나왔다 — footer 미출력. 아래 수치는 무의미하다.")

# 분포가 이봉이다. 짧은 쪽은 릴레이 한 번 안에서 지우고 다시 그리는 틈(대상)이고, 긴 쪽은
# 샘플이 없어 footer 를 정당하게 안 그린 구간(무관)이다. 섞어 세면 몇 배로 부풀려진다.
SPLIT = 1e-3
short = sorted(d for d, _ in spans if d < SPLIT)
long_ = [d for d, _ in spans if d >= SPLIT]
tot = sum(short)

print(f"  복사 구간            : {elapsed*1000:.0f} ms, write {len(ev)}회")
print(f"  릴레이 틈            : {len(short)}건" + (
    f", 중앙값 {short[len(short)//2]*1e6:.0f} us, 합 {tot*1000:.1f} ms" if short else ""))
print(f"  footer 부재(무관)    : {len(long_)}건, 합 {sum(long_)*1000:.1f} ms")
if elapsed > 0:
    print(f"  바 없는 시간 비율    : {tot/elapsed*100:.3f} %")
    print(f"  60 Hz 환산           : 초당 {tot/elapsed*60:.2f} 프레임이 바 없이 그려진다")
print()
print("  " + ("릴레이가 한 프레임으로 나갔다." if not short
              else "지우기가 단독으로 나가 바가 사라진 순간이 있다."))
PYEOF

  echo
  echo "전체 트레이스: $TRACE  (--keep 을 주면 남는다)"
  exit 0
fi

cat <<'EOF'

무엇을 볼 것인가
  - 바가 깜빡이거나 한 순간 사라졌다 나타나는가
  - 로그 줄이 흐르는 동안 바 두 줄이 계속 화면 맨 아래에 붙어 있는가
  - 파일명/빈 줄이 로그 사이에 끼어 쌓이지 않는가

Ctrl-C 로 중단해도 된다 (footer 는 정리된다).
EOF
read -r -p $'\n엔터를 누르면 시작한다... '

"$BINARY" -rv "$WORK/src" "$WORK/dst" || true

echo
echo "복사된 파일: $(find "$WORK/dst" -type f 2>/dev/null | wc -l) / $COUNT"
