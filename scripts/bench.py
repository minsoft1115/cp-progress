#!/usr/bin/env python3
"""cprog 오버헤드 실측.

디스크를 방정식에서 빼려고 tmpfs(/dev/shm)에서 복사한다. 남는 차이는 전부 cprog 것이다.
벽시계뿐 아니라 프로세스 트리 전체의 CPU 시간(user+sys)을 os.wait4로 받아 함께 본다 —
추가된 작업량이 직접 드러나고 노이즈가 훨씬 적다.

비용을 변형별로 분해한다:
  (1) cp                    기준선
  (2) stdbuf -oL cp -v      -v 주입 + 라인버퍼 강제 비용 (cp/stdbuf가 내는 비용, cprog가 아님)
  (3) cprog OLD             비교 기준 바이너리 (기본 /tmp/cprog-old, CPROG_OLD로 변경)
  (4) cprog NEW (-v 없이)   위 전부 + cprog 고유 비용   ->  (4)-(2) 가 순수 오버헤드
  (5) cprog NEW -v          사용자가 중계를 요청한 경우의 비용

모든 변형이 PTY에 출력한다. 한쪽만 /dev/null로 두면 "PTY vs /dev/null"을 재게 된다.
"""
import os, pty, select, shutil, statistics, subprocess, sys, threading, time, fcntl, struct, termios

# 비교할 두 바이너리. 회귀를 볼 때는 OLD 를 직전 릴리스 태그로 빌드해 둔다:
#   git checkout v0.3.0 && cargo build --release && cp target/release/cprog /tmp/cprog-old
#   git checkout main   && cargo build --release && cp target/release/cprog /tmp/cprog-new
OLD = os.environ.get("CPROG_OLD", "/tmp/cprog-old")
NEW = os.environ.get("CPROG_NEW", "/tmp/cprog-new")
ROOT = "/dev/shm/cprog-bench"
REPS = 7

def drain(master):
    """PTY를 계속 비운다. S2는 -v 줄이 2만 개라 안 비우면 자식이 막힌다."""
    total = 0
    while True:
        try:
            r, _, _ = select.select([master], [], [], 30)
            if not r:
                break
            b = os.read(master, 1 << 16)
            if not b:
                break
            total += len(b)
        except OSError:
            break
    return total

def run(argv, env_extra=None):
    """argv를 PTY에 붙여 실행하고 (벽시계, user CPU, sys CPU)를 돌려준다."""
    m, s = pty.openpty()
    fcntl.ioctl(s, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 100, 0, 0))
    env = dict(os.environ, TERM="xterm", LC_ALL="C.UTF-8")
    env.pop("CI", None)
    if env_extra:
        env.update(env_extra)

    t0 = time.monotonic()
    pid = os.fork()
    if pid == 0:
        os.dup2(s, 0); os.dup2(s, 1); os.dup2(s, 2)
        if s > 2:
            os.close(s)
        os.close(m)
        try:
            os.execvpe(argv[0], argv, env)
        finally:
            os._exit(127)
    os.close(s)
    t = threading.Thread(target=drain, args=(m,), daemon=True)
    t.start()
    _, status, ru = os.wait4(pid, 0)
    wall = time.monotonic() - t0
    t.join(timeout=5)
    os.close(m)
    return wall, ru.ru_utime, ru.ru_stime

# ---- 시나리오 준비 ------------------------------------------------------------

def prepare(scenario):
    src = os.path.join(ROOT, "src")
    shutil.rmtree(ROOT, ignore_errors=True)
    os.makedirs(src)
    if scenario == "S1":
        # 4 GiB 단일 파일: 실제로 진행바가 쓰이는 상황
        with open(os.path.join(src, "big.bin"), "wb") as f:
            chunk = os.urandom(1 << 20)
            for _ in range(4096):
                f.write(chunk)
        return [os.path.join(src, "big.bin")], "파일 1개 × 4 GiB"
    else:
        # 4 KiB 파일 20,000개: 최악의 경우 (-v 2만 줄 + relay + 터미널 출력)
        blob = os.urandom(4096)
        for i in range(20000):
            d = os.path.join(src, f"d{i // 1000:02d}")
            os.makedirs(d, exist_ok=True)
            with open(os.path.join(d, f"f{i:05d}.bin"), "wb") as f:
                f.write(blob)
        return [src], "파일 20,000개 × 4 KiB (총 78 MiB)"

VARIANTS = [
    ("1 cp",                   lambda srcs, dst: ["cp", "-r", *srcs, dst]),
    ("2 stdbuf -oL cp -v",     lambda srcs, dst: ["stdbuf", "-oL", "cp", "-rv", *srcs, dst]),
    ("3 cprog OLD",         lambda srcs, dst: [OLD, "-r", *srcs, dst]),
    ("4 cprog NEW (-v 없이)",   lambda srcs, dst: [NEW, "-r", *srcs, dst]),
    ("5 cprog NEW -v",         lambda srcs, dst: [NEW, "-rv", *srcs, dst]),
]

def main():
    for scenario in ("S1", "S2"):
        srcs, label = prepare(scenario)
        print(f"\n{'='*78}\n{scenario}: {label}   (tmpfs, {REPS}회 교대 반복)\n{'='*78}")
        results = {name: [] for name, _ in VARIANTS}
        for rep in range(REPS):
            for name, build in VARIANTS:          # 교대 실행: A-B-C-D-A-B-C-D…
                dst = os.path.join(ROOT, "dst")
                shutil.rmtree(dst, ignore_errors=True)
                os.makedirs(dst)
                results[name].append(run(build(srcs, dst)))
                shutil.rmtree(dst, ignore_errors=True)
            print(f"  … {rep+1}/{REPS}", flush=True)

        print(f"\n  {'변형':<22} {'벽시계(중앙값)':>14} {'user CPU':>10} {'sys CPU':>10} {'CPU 합':>10}")
        print("  " + "-"*70)
        base = None
        for name, _ in VARIANTS:
            w = statistics.median(r[0] for r in results[name])
            u = statistics.median(r[1] for r in results[name])
            s = statistics.median(r[2] for r in results[name])
            cpu = u + s
            if base is None:
                base = cpu
            print(f"  {name:<22} {w:>13.3f}s {u:>9.3f}s {s:>9.3f}s {cpu:>9.3f}s")
        # 순수 cprog 오버헤드 = (4) - (3)
        base = statistics.median(r[1]+r[2] for r in results["2 stdbuf -oL cp -v"])
        w1 = statistics.median(r[0] for r in results["1 cp"])
        print()
        for name, _ in VARIANTS[2:]:  # cprog 변형만; VARIANTS 이름을 그대로 써 키가 어긋날 수 없다
            cpu = statistics.median(r[1]+r[2] for r in results[name])
            w = statistics.median(r[0] for r in results[name])
            print(f"  ► {name:<22} 고유 CPU {cpu-base:+.3f}s   벽시계 {w:.3f}s ({(w/w1-1)*100:+.1f}% vs cp)")
    shutil.rmtree(ROOT, ignore_errors=True)

main()

