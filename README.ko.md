# moraebox

[English](README.md)

[![GitHub release](https://img.shields.io/github/v/release/qkdxorjs1002/moraebox?include_prereleases)](https://github.com/qkdxorjs1002/moraebox/releases)
[![Rust 1.85+](https://img.shields.io/badge/Rust-1.85%2B-000000?logo=rust&logoColor=white)](Cargo.toml)
[![CI](https://github.com/qkdxorjs1002/moraebox/actions/workflows/ci.yml/badge.svg)](https://github.com/qkdxorjs1002/moraebox/actions/workflows/ci.yml)

**코딩 에이전트를 위한 일회용 microVM 실행 환경.** moraebox는 데몬 없이 동작하는 Rust 런타임입니다. 일회성 명령마다 독립된 Linux microVM을 만들고 출력을 스트리밍하며, 실행 완료·타임아웃·취소·백엔드 실패·소유자 프로세스 종료 시 샌드박스를 폐기합니다.

Phase 0–5 수직 기능 묶음이 구현되어 있습니다. 네이티브 실행은 호환되는 정식 libkrun 및 libkrunfw 빌드를 갖춘 Apple Silicon macOS에서만 현재 릴리스 검증을 마쳤습니다. Linux와 Windows는 컴파일·테스트 대상이며, process 백엔드는 결정론적 테스트 대역일 뿐 **VM 격리를 제공하지 않습니다**.

[빠른 시작](#빠른-시작) · [설치](#설치) · [샌드박스 실행](#샌드박스-실행) · [MCP 서버](#mcp-서버) · [보안 모델](#보안-모델)

## 왜 moraebox인가?

코딩 에이전트 하네스에는 단순한 자식 프로세스보다 강한 경계가 필요하지만, 장시간 실행되는 권한 데몬이나 신뢰할 수 없는 상태를 보존하는 재사용 VM까지 필요하지는 않습니다.

| 요구 사항 | moraebox 동작 |
| --- | --- |
| 명확한 일회성 소유권 | 하나의 helper 프로세스와 새 microVM 하나가 샌드박스 실행 하나에만 속합니다 |
| 예측 가능한 정리 | 완료, 타임아웃, 취소, 실패, 부모 프로세스 종료가 모두 정리 단계로 수렴합니다 |
| 안전한 명령 처리 | 명령은 argv 배열이며 암묵적인 셸 파싱을 추가하지 않습니다 |
| 보수적인 호스트 접근 | 호스트 워크스페이스를 직접 virtio-fs로 노출하지 않고 불변 ext4 이미지로 변환합니다 |
| 제한된 실행 | 기본 벽시계 타임아웃은 1시간이며 무제한 실행은 명시적으로 지정합니다 |
| 에이전트 친화적 통합 | CLI, 비동기 Rust SDK, stdio MCP 서버가 같은 수명주기 모델을 공유합니다 |

## 빠른 시작

### Homebrew로 설치

현재 Homebrew 릴리스 대상은 Apple Silicon macOS입니다.

```sh
brew tap qkdxorjs1002/tap
brew trust qkdxorjs1002/tap

# 안정 릴리스
brew install moraebox

# 최신 안정 또는 prerelease
brew install moraebox@pre

morae --version
morae doctor --json
```

Homebrew 6은 타사 formula에 명시적인 신뢰를 요구합니다. 이 tap을 신뢰하면 moraebox 설치 중 고정 companion dependency formula를 함께 해석할 수 있습니다.

두 formula는 checksum으로 검증된 릴리스 소스 아카이브를 내려받아 `brew install`을 실행한 Mac에서 `morae`, `morae-mcp`, `morae-vmm-helper`를 컴파일합니다. 미리 빌드된 moraebox bottle이나 바이너리는 내려받지 않습니다. 같은 설치에서 이 tap의 companion formula를 통해 고정된 `libkrun` 1.19.4와 `libkrunfw` 5.5.0 native runtime도 제공합니다. Homebrew가 빌드 의존성과 `e2fsprogs`를 설치하며, 로컬에서 빌드한 helper에는 Hypervisor entitlement를 포함한 ad-hoc 서명을 적용합니다. 이 서명은 개발자 신원을 나타내지 않으며 Apple 공증을 받지 않습니다.

안정 및 prerelease formula는 같은 실행 파일을 설치하므로 서로 충돌합니다. 채널을 바꾸기 전에 현재 formula를 제거해야 합니다. `doctor`는 호스트를 변경하지 않고 누락된 경로, 심볼, 프레임워크, 서명 기능을 정확히 보고합니다.

### 이식 가능한 실행 경로 확인

```sh
morae run --backend process -- /usr/bin/printf 'hello from moraebox\n'
```

이 명령은 수명주기와 출력 경로만 확인합니다. process 백엔드는 호스트에서 직접 실행되며 **보안 샌드박스가 아닙니다**.

### 네이티브 microVM 실행

Homebrew 설치가 검증된 native library와 서명된 sibling helper를 제공합니다. moraebox가 이를 자동 탐색하므로 별도의 셸 설정이 필요하지 않습니다.

```sh
morae doctor --strict
morae run --image alpine@latest -- /bin/uname -a
```

`--helper`, `--libkrun`, `MORAE_HELPER_PATH`, `MORAE_LIBKRUN_PATH`, `MORAE_LIBKRUNFW_PATH`, `MORAE_LIB_DIR`는 비표준 설치를 위한 명시적 override로 계속 사용할 수 있습니다.

현재 검증된 개발 스택은 Apple Silicon macOS의 libkrun 1.19.4와 libkrunfw 5.5.0입니다. 어댑터는 정식 libkrun 1.x root API와 명시적 리소스를 사용하는 2.0 ABI를 런타임에 감지합니다. 아직 릴리스되지 않은 `main` 브랜치 ABI 변경은 호환 대상이 아닙니다.

## 설치

### 요구 사항

네이티브 실행:

- Hypervisor.framework를 제공하는 Apple Silicon macOS
- 호환되는 정식 libkrun 및 libkrunfw 빌드(Homebrew formula가 자동 설치)
- `com.apple.security.hypervisor` entitlement로 서명된 helper
- 호스트 워크스페이스 연결 시 `e2fsprogs`의 `mke2fs`
- 이미지 pull 시 선택한 OCI 레지스트리에 대한 네트워크 접근

소스 개발:

- Rust 1.85 이상
- Rust가 요구하는 플랫폼 빌드 도구
- 실제 백엔드 검사에 한해 위 네이티브 의존성

### 소스에서 빌드

```sh
cargo build --release --locked \
  -p moraebox-cli \
  -p moraebox-mcp \
  -p moraebox-vmm-helper

codesign --force --sign - \
  --entitlements assets/moraebox-vmm.entitlements \
  target/release/morae-vmm-helper
```

Homebrew formula도 같은 서명 모델을 사용합니다. 설치하는 Mac에서 helper를 빌드하고 ad-hoc 서명하며, 필요한 entitlement는 포함하지만 Developer ID 신원과 Apple 공증은 제공하지 않습니다.

## 샌드박스 실행

### 네이티브 준비 상태 확인

```sh
morae doctor
morae doctor --json
morae doctor --strict
```

`--strict`는 네이티브 백엔드가 준비되지 않았으면 실패 종료 코드를 반환합니다.

### OCI 이미지 pull

```sh
morae image pull alpine@latest --json
morae image pull ghcr.io/example/image:tag \
  --cache-dir .moraebox/cache
```

레지스트리 manifest와 blob은 레이어를 구체화하기 전에 digest를 검증합니다. 레지스트리 인증 정보는 옵션 또는 `MORAE_REGISTRY_USERNAME`과 `MORAE_REGISTRY_PASSWORD`를 통해 명시적인 사용자 이름/비밀번호 쌍으로만 받습니다.

이미지 계층은 `oci-layout:`과 `docker-archive:` 참조를 파싱하지만 공개 CLI는 아직 이를 가져오지 않습니다.

### 명령 실행

```sh
morae run \
  --image alpine@latest \
  --cpus 2 \
  --memory-mib 512 \
  --timeout 30s \
  -- /bin/echo hello
```

`--` 뒤의 모든 값은 argv 배열로 전달됩니다. 셸을 직접 실행할 때만 셸 문법이 해석됩니다.

```sh
morae run --image alpine@latest -- /bin/sh -c 'printf "%s\n" "$HOME"'
```

게스트 환경은 기본적으로 비어 있습니다. `--env KEY=VALUE`로 값을 개별 추가하고, 호스트 환경 전달이 의도된 경우에만 `--inherit-env`를 사용합니다.

기본 타임아웃은 1시간입니다.

```sh
morae run --backend process --timeout 10m -- /usr/bin/true
morae run --backend process --timeout none -- /usr/bin/true
```

`none` 또는 `0`이 명시적인 무제한 설정입니다.

### 읽기 전용 워크스페이스 연결

```sh
morae run \
  --rootfs /path/to/materialized-rootfs \
  --workspace ./project \
  -- /bin/sh -c 'cat /workspace/Cargo.toml'
```

moraebox는 심볼릭 링크를 따라가지 않고 호스트 트리를 순회하며, 안전하지 않은 항목을 거부한 뒤 mode 0444 ext4 이미지를 생성해 `/workspace`에 읽기 전용으로 연결합니다. 원본 호스트 디렉터리를 virtio-fs로 직접 노출하지 않습니다.

### PTY로 스트리밍

```sh
morae run --image alpine@latest --tty --interactive -- /bin/sh
```

PTY 할당은 네이티브 백엔드에서 사용할 수 있습니다. macOS 컨트롤러의 실시간 PTY 크기 조절은 아직 구현되지 않았습니다.

### 수명주기 벤치마크

```sh
morae benchmark \
  --backend process \
  --iterations 100 \
  -- /usr/bin/true
```

JSON 보고서에는 최소, p50, p95, p99, 최대 지연 시간이 포함됩니다. 현재 준비 풀은 부팅된 guest-agent VM이 아니라 검증·구체화된 아티팩트를 캐시하므로 보고서에서 이 모드를 `cached-cold`라고 부릅니다.

## MCP 서버

두 백엔드 중 하나로 줄바꿈 구분 stdio 서버를 시작합니다.

```sh
morae-mcp --backend process

MORAE_ROOTFS="/path/to/materialized-rootfs" \
morae-mcp --backend libkrun
```

MCP 서버는 stdout을 프로토콜 메시지 전용으로 유지하며 진단 메시지는 stderr로 보냅니다.

| 도구 | 용도 |
| --- | --- |
| `sandbox_exec` | 일회성 명령 또는 비동기 세션 시작 |
| `sandbox_io` | cursor 기반 출력 읽기, stdin 쓰기·닫기, 크기 조절, signal 전송 |
| `sandbox_stop` | 세션을 중지하고 정리가 끝날 때까지 대기 |

MCP 스키마에서도 명령은 argv 배열입니다. 출력 바이트와 stdin은 base64로 인코딩되며 출력 읽기 크기는 제한됩니다.

## 아키텍처

```text
CLI / Rust SDK / MCP
          |
   runtime supervisor
상태, deadline, I/O, cleanup
          |
 VM별 vmm-helper 프로세스
     안정된 libkrun ABI
          |
 console + vsock (TSI off)
          |
   일회성 Linux microVM
```

`krun_start_enter()`가 libkrun context를 소비하고 호출 프로세스를 종료하므로 helper는 별도 프로세스 경계입니다. CLI, SDK 호스트, MCP 서버 밖에 helper를 두면 supervisor가 명확한 소유권 handle을 가질 수 있습니다.

공개 일회성 수명주기는 다음과 같습니다.

```text
New → Preparing → Starting → Ready → Running → Stopping → Dead
                      \          \       \
                       └─────────── Failed ─────→ Dead
                                   TimedOut ────→ Dead
```

`Dead`는 프로세스 handle, control socket, I/O pump, 임시 파일, VM 리소스가 회수되었다는 뜻입니다. crate 소유권과 저장소 흐름은 [아키텍처 문서](docs/architecture.md)를 참고하세요.

## 보안 모델

v1 위협 모델은 로컬 사용자가 자신의 macOS 호스트에서 호출한 신뢰할 수 없는 Linux 게스트 코드입니다. 적대적인 호스트 사용자, 손상된 hypervisor/VMM, 적대적인 멀티테넌트 환경으로부터의 보호를 주장하지 않습니다.

보안 기본값:

- 게스트 네트워크 인터페이스를 기본으로 추가하지 않습니다.
- Transparent Socket Impersonation 플래그를 0으로 설정해 control vsock을 생성합니다.
- 호스트 소스 디렉터리를 불변 읽기 전용 block 이미지로 변환합니다.
- 게스트 환경은 빈 상태에서 시작합니다.
- 명령에 암묵적인 셸 파싱을 적용하지 않습니다.
- 기본 벽시계 제한 시간은 1시간입니다.
- 중지는 TERM 이후 5초의 유예 시간을 거쳐 KILL로 승격합니다.
- OCI 콘텐츠의 digest를 검증하고 레이어에서 traversal, device, 안전하지 않은 link, symlink parent 탈출을 거부합니다.
- 준비된 unit은 한 번만 소비하며 lease 종료 후 신뢰할 수 없는 VM을 재사용하지 않습니다.
- 부모 프로세스 종료 시 helper를 종료하고 정리 단계로 수렴합니다.

신뢰 경계는 [docs/security.md](docs/security.md), 제한된 host/guest frame 계약은 [docs/protocol.md](docs/protocol.md)를 참고하세요.

## 현재 제약

- 네이티브 실행은 Apple Silicon macOS에서만 릴리스 검증을 마쳤습니다.
- Linux와 Windows는 CI에서 컴파일·테스트하지만 네이티브 릴리스 바이너리는 제공하지 않습니다.
- process 백엔드는 결정론적 테스트용이며 격리를 제공하지 않습니다.
- 레지스트리 이미지는 구체화하지만 로컬 OCI layout과 Docker archive는 아직 가져오지 않습니다.
- 네이티브 root filesystem은 전용으로 구체화한 virtio-fs 디렉터리를 사용하고, 호스트 워크스페이스는 읽기 전용 block 이미지를 사용합니다.
- 쓰기 가능한 workspace overlay, copy-out/diff, 사용자 정의 guest-agent handshake, 실시간 PTY resize는 후속 작업입니다.
- 아티팩트 풀은 `cached-cold`이며 이미 부팅된 재사용 VM 풀을 의미하지 않습니다.

## 개발

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

네이티브 macOS 변경은 다음 검사도 필요합니다.

```sh
morae doctor --json
```

서명된 helper, 정식 libkrun/libkrunfw 빌드, Hypervisor.framework 기능을 사용할 수 있으면 실제 백엔드 smoke suite를 실행합니다. 네이티브 검사를 생략할 때는 누락된 기능을 정확히 밝혀야 합니다.

프로젝트 상세 문서:

- [구현 계획](docs/implementation-plan.md)
- [아키텍처](docs/architecture.md)
- [보안 모델](docs/security.md)
- [프로토콜](docs/protocol.md)
- [성능](docs/performance.md)

## 릴리스

앞에 `v`가 없는 검증된 태그를 push하면 [릴리스 워크플로](.github/workflows/release.yml)가 시작됩니다. 안정 태그는 `x.y.z`, prerelease 태그는 `x.y.z-alphaN`, `x.y.z-betaN`, `x.y.z-rcN` 형식이며 예시는 `0.0.0-alpha1`입니다. 태그가 릴리스 버전의 기준이며 품질 게이트 전에 runner의 임시 workspace에 동기화됩니다. 소스 commit은 다시 쓰지 않습니다.

1. Apple Silicon macOS runner에서 전체 Rust 품질 게이트를 실행합니다.
2. 동기화한 workspace manifest와 잠긴 의존성 집합을 포함하는 버전별 소스 아카이브를 만들고 내용을 검증합니다.
3. GitHub Releases에는 소스 아카이브와 SHA-256 파일만 게시하고 prerelease 태그를 그에 맞게 표시합니다.
4. `qkdxorjs1002/homebrew-tap`의 고정 `Formula/libkrun.rb`와 `Formula/libkrunfw.rb`, rolling `Formula/moraebox-pre.rb`, `moraebox@pre` alias를 갱신하며, 안정 태그일 때는 `Formula/moraebox.rb`도 갱신합니다.
5. 각 `brew install`에서 Homebrew가 고정 native 의존성을 설치하고 libkrun과 moraebox 실행 파일 세 개를 해당 Mac에서 빌드한 뒤, 설치된 VMM helper에 `assets/moraebox-vmm.entitlements`를 사용한 ad-hoc 서명을 적용합니다.

워크플로는 moraebox 바이너리를 게시하지 않으며 Developer ID 서명과 Apple 공증도 사용하지 않습니다. 따라서 설치할 때마다 소스를 직접 빌드하며, 결과의 ad-hoc 서명은 개발자 신원이나 공증을 증명하지 않습니다.

저장소 릴리스 secret:

| Secret | 용도 |
| --- | --- |
| `HOMEBREW_TAP_TOKEN` | `qkdxorjs1002/homebrew-tap` 쓰기 권한 |

## 라이선스

moraebox는 Apache-2.0 라이선스를 따릅니다.
