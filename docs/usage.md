# Usage (사용법)

## 명령 형태

```
cprog <cp args...>
```

`cprog`는 자체 서브커맨드·옵션 체계가 없다. interactive 감지 정도만 인자를 보고, 나머지는
`cp`에 그대로 넘긴다. 인자가 없으면 usage 한 줄 출력 후 `1`로 종료.

## 예제

```bash
cprog big.iso /mnt/backup/big.iso      # 큰 파일 → 느려지면 진행바
cprog -r ./photos ./backup             # 재귀 → -v 스크롤, 큰 파일만 바
cprog -a ./project ./project.bak       # 속성 보존(복사는 cp가 함)
```

모든 플래그는 `cp`로 그대로 전달된다.

## `cp` 대체하기

```bash
cargo install --path .
echo "alias cp='cprog'" >> ~/.bashrc && source ~/.bashrc
cp big.iso /mnt/backup/big.iso       # 터미널이면 진행바가 뜸
```

managed 모드는 `cp` 인자를 (‑v 외에) 안 바꾸고, passthrough에서는 스트림을 inherit하므로,
스크립트·파이프·리다이렉트에서 alias는 안전하다.

## interactive 복사

interactive 플래그는 passthrough(‑ footer·캡처 없음)를 강제해 덮어쓰기 프롬프트가 정상
동작한다.

```bash
cprog -i big.iso backup.iso
```

## 바가 안 뜨는 경우 (의도된 동작)

- 출력 리다이렉트: `cprog a b > log` (stdout이 TTY 아님)
- `cprog a b 2> err` (stderr가 TTY 아님)
- stdout/stderr가 다른 터미널
- `TERM=dumb` 또는 CI
- 비-리눅스(‑ `/proc` 없음)
- `stdbuf`가 없음(‑ `-v`를 실시간으로 못 흘려서 → passthrough)
- 백그라운드 실행: `cprog a b &` (전경 프로세스 그룹이 아니면 터미널을 점거하지 않음).
  Ctrl-Z 후 `bg`로 백그라운드 재개한 경우도 이후 footer를 그리지 않는다(‑ `fg`로 되돌려도
  그 실행에서는 꺼진 채 유지; 다시 Ctrl-Z 후 `fg` 하면 복구).

이 모든 경우 `cprog`는 `cp`와 바이트 동일.

## 바가 뜨는 경우

- 리눅스 대화형 터미널에서, **한 파일 복사가 100ms를 넘길 때만** 그 파일의 진행바가
  footer에 뜬다. 작은 파일이 많으면 `-v` 줄이 위에서 스크롤될 뿐(‑ 바 없음).

## 종료 동작

- 정상: `cp`의 exit code 그대로.
- 시그널(Ctrl-C 등): `cp`의 signaled-exit 의미론 보존, 요약 없음.
- `cprog` 쪽 문제(샘플/렌더/정리): 경고까지만 — exit code는 언제나 `cp`에서.

## 환경 변수

| 변수 | 효과 |
|---|---|
| `NO_COLOR` | footer 색 끔(설정만 돼 있으면 값 무관) |
| `CPROG_SLOW_THRESHOLD_MS` | 느린 파일 판정 임계(기본 100) |
| `CPROG_SAMPLE_INTERVAL_MS` | `stat` 폴링 주기(기본 100) |
| `CPROG_RENDER_TICK_MS` | footer 리드로우 tick(기본 125) |

전부 안전한 기본값이 있고, 필수는 없다. `CPROG_*_MS` 값이 숫자가 아니거나 파싱에 실패하면
에러 없이 **조용히 기본값으로 폴백**한다. footer는 항상 한 줄이라 높이 상한 옵션은 없다(로그
영역으로 `min_log_rows`=2행을 항상 남긴다).
