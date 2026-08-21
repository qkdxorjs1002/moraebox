<p align="center">
  <img src="assets/moraebox.png" alt="모래성 모양의 moraebox 로고" width="320">
</p>

# moraebox

[English](README.md)

[![GitHub release](https://img.shields.io/github/v/release/qkdxorjs1002/moraebox?include_prereleases)](https://github.com/qkdxorjs1002/moraebox/releases)
[![CI](https://github.com/qkdxorjs1002/moraebox/actions/workflows/ci.yml/badge.svg)](https://github.com/qkdxorjs1002/moraebox/actions/workflows/ci.yml)
[![Rust 1.85+](https://img.shields.io/badge/Rust-1.85%2B-000000?logo=rust&logoColor=white)](Cargo.toml)
[![License: Apache-2.0](https://img.shields.io/badge/License-Apache--2.0-blue.svg)](#라이선스)

**코딩 에이전트의 명령마다 새로 만드는 일회용 Linux microVM.**

moraebox는 데몬 없이 동작하는 Rust 런타임입니다. 명령 하나를 위해 새 microVM 하나를 시작하고 출력을 스트리밍한 뒤, 명령이 끝나거나 타임아웃·소유자·백엔드에 문제가 생기면 샌드박스를 정리합니다.

> [!IMPORTANT]
> 현재 네이티브 microVM 실행의 릴리스 검증 대상은 Apple Silicon macOS입니다. 이식 가능한 `process` 백엔드는 테스트·개발용이며, 명령을 호스트에서 직접 실행하므로 **VM 격리를 제공하지 않습니다**.

## moraebox가 필요한 이유

코딩 에이전트는 낯선 코드를 실행하고 의존성을 설치하며 빌드 도구를 호출합니다. 자식 프로세스는 편리하지만 격리 경계가 아닙니다. 반대로 장시간 유지되는 VM은 작업 사이에 신뢰할 수 없는 상태를 남길 수 있습니다.

moraebox는 더 작은 수명주기를 지향합니다. 소유자 하나, 명령 하나, VM 하나, 그리고 정리입니다.

- **일회용 설계.** 준비된 샌드박스는 한 번만 소비하며 신뢰할 수 없는 VM 풀로 되돌리지 않습니다.
- **모든 종료 경로에서 정리.** 성공, 타임아웃, 취소, 백엔드 실패, 부모 프로세스 종료가 모두 teardown으로 수렴합니다.
- **숨겨진 셸 없음.** 셸을 직접 실행하지 않는 한 명령은 argv 배열로 유지됩니다.
- **호스트 환경 미상속.** 게스트 환경은 기본적으로 비어 있습니다.
- **보수적인 워크스페이스 접근.** 호스트 소스 트리를 직접 virtio-fs로 공유하지 않고 불변 읽기 전용 ext4 이미지로 만듭니다.
- **에이전트용 인터페이스.** CLI, 비동기 Rust SDK, stdio MCP 서버가 같은 런타임 수명주기를 사용합니다.

## 빠른 시작

### 1. 설치

현재 prerelease 채널은 Apple Silicon macOS를 대상으로 합니다. tap은 moraebox가 요구하는 고정 gvproxy, libkrun 및 libkrunfw 버전도 함께 제공합니다.

```sh
brew tap qkdxorjs1002/tap
brew trust --tap qkdxorjs1002/tap
brew install moraebox@pre
```

Homebrew 6은 비공식 tap에 신뢰 설정을 요구합니다. moraebox formula가 같은 tap의 companion formula에 의존하므로 여기서는 tap 전체를 신뢰합니다. 신뢰하기 전에 tap 내용을 검토하세요. 더 좁은 신뢰 옵션은 Homebrew의 [Tap Trust 문서](https://docs.brew.sh/Tap-Trust)를 참고할 수 있습니다.

formula는 checksum으로 검증한 릴리스 소스에서 `morae`, `morae-mcp`, `morae-vmm-helper`를 빌드합니다. helper에는 Hypervisor entitlement를 사용한 ad-hoc 서명도 적용합니다. 미리 빌드된 moraebox 바이너리는 설치하지 않습니다.

### 2. 네이티브 런타임 확인

```sh
morae --version
morae doctor --strict
```

`doctor`는 호스트를 변경하지 않습니다. `morae doctor --json`은 누락된 라이브러리, 심볼, 프레임워크, 도구, 서명 기능을 기계가 읽을 수 있는 형식으로 보고합니다.

### 3. 명령 실행

```sh
morae run -- python3 -c 'print("hello from moraebox")'
```

내장 기본 이미지는 `docker.io/library/python:3.12`입니다. 첫 실행에서 이미지를 pull하고 검증한 뒤 구체화한 로컬 캐시를 재사용합니다. 각 실행의 VM은 여전히 새로 만듭니다.

## CLI 사용법

### 리소스와 타임아웃 지정

```sh
morae run \
  --cpus 2 \
  --memory-mib 512 \
  --timeout 30s \
  -- python3 -c 'print("isolated")'
```

기본 벽시계 타임아웃은 1시간입니다. 무제한 실행은 `--timeout none` 또는 `--timeout 0`으로 명시해야 합니다.

`--` 뒤의 값은 모두 argv로 전달됩니다. 셸을 명령으로 직접 지정한 경우에만 셸 문법을 해석합니다.

```sh
morae run --image alpine:latest --env MESSAGE=hello \
  -- /bin/sh -c 'printf "%s\n" "$MESSAGE"'
```

개별 환경값은 `--env KEY=VALUE`로 추가합니다. `--inherit-env`는 호스트 환경을 전달하므로 해당 노출이 의도된 경우에만 사용해야 합니다.

### 외부 네트워크 접속 허용

게스트 네트워크는 기본적으로 꺼져 있습니다. 네이티브 VM 실행 하나에만 허용하려면 `--network`를 지정합니다.

```sh
morae run --network -- curl -I https://example.com
```

네트워크 실행에는 Homebrew formula가 자동으로 설치하는 `gvproxy`가 필요합니다. moraebox는 `PATH`에서 `gvproxy`를 찾으며, Homebrew 외의 방식으로 다른 위치에 설치했다면 `--gvproxy /path/to/gvproxy` 또는 `MORAE_GVPROXY_PATH`를 사용합니다. 네이티브 런타임은 실행마다 새 gvproxy 프로세스와 virtio-net endpoint를 시작하고 VM과 함께 정리합니다. control vsock은 별도로 유지하며 모든 TSI feature flag를 끕니다.

Rust SDK에서는 `RunSpec.network = true`, MCP `sandbox_exec`에서는 `"network": true`가 같은 실행별 opt-in입니다. `process` 백엔드는 호스트 네트워크 문맥에서 직접 실행되고 VM 격리를 제공하지 않으므로 이 VM 전용 옵션을 거부합니다.

네트워크를 켜면 게스트 코드는 호스트 사용자 네트워크 문맥에서 가능한 외부 접속을 사용할 수 있습니다. 목적지 allowlist나 별도 네트워크 보안 경계는 아닙니다.

### OCI 이미지 선택

```sh
# 이번 실행에서만 다른 이미지 사용
morae run --image debian:bookworm -- cat /etc/os-release

# 이후 실행의 기본 이미지 변경
morae image default debian:bookworm

# 내장 python:3.12 기본값 복원
morae image default --unset
```

레지스트리 manifest와 blob은 레이어를 구체화하기 전에 digest를 검증합니다. 비공개 레지스트리는 CLI 옵션 또는 `MORAE_REGISTRY_USERNAME`과 `MORAE_REGISTRY_PASSWORD`로 명시적인 사용자 이름/비밀번호 쌍을 받습니다.

`--rootfs /path/to/rootfs`는 이미 구체화한 guest root 디렉터리를 사용하는 고급 대안입니다. 이미지 해석을 우회하며 `--image`와 함께 사용할 수 없습니다.

### Persistent Box로 작업 이어가기

일반 실행은 새 `SessionId`, 새 microVM, 정리 후 삭제되는 ephemeral copy-on-write root disk를 받습니다. 이후 실행에서도 파일 변경을 유지해야 할 때만 Box를 만듭니다.

```sh
BOX_ID=$(morae box create --image alpine:latest --json | jq -r .box_id)

morae run --box "$BOX_ID" -- /bin/sh -c 'echo retained > /root/result'
morae run --box "$BOX_ID" -- cat /root/result

morae box clone "$BOX_ID" --yes
morae box reset "$BOX_ID" --yes
morae box delete "$BOX_ID" --yes
```

`BoxId`는 persistent root filesystem 계보를 식별하며 VM identity나 인증 수단이 아닙니다. `morae run --box`도 매번 새 microVM과 `SessionId`를 만들고 Box disk의 파일만 이어받습니다. 서로 다른 Box는 독립된 disk를 사용하며 같은 Box의 두 번째 writer는 즉시 실패합니다. `--box`는 `--image`, `--rootfs`, `--workspace`와 함께 사용할 수 없고, 격리 없는 `process` 백엔드는 이를 거부합니다.

### 읽기 전용 워크스페이스 연결

```sh
morae run \
  --workspace ./my-project \
  -- /bin/sh -c 'ls -la /workspace'
```

moraebox는 심볼릭 링크를 따라가지 않고 호스트 트리를 순회하며, 안전하지 않은 항목을 거부한 뒤 읽기 전용 ext4 스냅샷을 만들어 `/workspace`에 연결합니다. 원본 호스트 디렉터리는 VM에 노출하지 않습니다.

### 대화형 터미널 사용

```sh
morae run --image alpine:latest --tty --interactive -- /bin/sh
```

네이티브 백엔드는 PTY 할당을 지원합니다. 실시간 터미널 크기 조절은 아직 구현되지 않았습니다.

### 로컬 저장공간 관리

```sh
morae image pull python:3.12
morae image list
morae image remove python:3.12

morae box create --image python:3.12
morae box list
morae box show BOX_ID
morae box clone BOX_ID --yes
morae box reset BOX_ID --yes
morae box delete BOX_ID --yes

morae cache info
morae cache prune --dry-run
morae cache prune --yes
morae cache clean --all --dry-run
morae cache clean --all --yes
```

모든 명령은 현재 작업 디렉터리와 무관하게 사용자 전역 `~/.moraebox/cache`와 `~/.moraebox/state`를 기본으로 사용합니다. 다른 위치가 필요한 명령에만 `--cache-dir` 또는 `--state-dir`을 지정합니다. 기존 프로젝트 로컬 데이터는 자동으로 옮기지 않으며, 예를 들어 `morae box list --state-dir .moraebox/state`로 계속 사용할 수 있습니다.

파괴적인 캐시 작업에는 `--dry-run` 또는 `--yes`가 필요하며 durable Box 변경에는 `--yes`가 필요합니다. `morae cache clean`은 다시 만들 수 있는 image, immutable base disk, ephemeral 데이터만 지우고 `~/.moraebox/state`의 persistent Box disk는 지우지 않습니다. 구조화된 출력이 필요한 image·Box·cache 명령은 `--json`을 지원합니다.

### 격리 없이 수명주기만 확인

```sh
morae run --backend process -- /usr/bin/printf 'portable path\n'
morae benchmark --backend process --iterations 100 -- /usr/bin/true
```

`morae run`, `morae benchmark`, `morae-mcp`는 기본적으로 `libkrun` microVM 백엔드를 사용합니다. `process` 백엔드는 위 예시처럼 `--backend process`를 명시한 개발용 opt-in으로만 사용할 수 있습니다. 결정론적 테스트, CI, 통합 개발에는 유용하지만 샌드박스가 아니며 샌드박스로 설명해서도 안 됩니다. `--image`, `--rootfs`, `--box` 같은 guest root 옵션은 process 백엔드에서 무시되지 않고 오류로 거부됩니다.

네이티브 cached-start를 검증할 때는 첫 guest 출력을 보수적인 command-start 신호로 측정할 수 있도록 즉시 출력하는 명령을 사용합니다.

```sh
morae benchmark --image alpine:latest \
  --iterations 100 -- /bin/echo ready
```

JSON report는 immutable-base 조회, Box lock, CoW clone, root 준비, helper spawn, 첫 guest 출력, 전체 완료 percentile을 분리합니다. Native 실행은 `mode: "cached-one-shot"`, 명시적 process benchmark는 `mode: "host-process"`로 표시하므로 호스트 실행을 microVM 성능으로 오인하지 않습니다.

## 코딩 에이전트 연결

`morae-mcp`는 줄바꿈으로 구분하는 stdio MCP 서버입니다. stdout은 프로토콜 메시지 전용이며 진단 정보는 stderr로 보냅니다.

Codex 또는 Claude Code에 네이티브 서버를 등록합니다.

```sh
morae-mcp install codex
morae-mcp install claude-code
```

에이전트 설정을 바꾸지 않고 정확한 command와 argv를 미리 확인할 수 있습니다.

```sh
morae-mcp install codex --dry-run
```

설치기는 에이전트의 공식 CLI를 사용하며 설정 파일을 직접 편집하지 않습니다. 서버 실행 파일, 저장소, rootfs, 발견한 native dependency 경로를 절대경로로 기록하므로 에이전트의 작업 디렉터리나 `PATH`에 의존하지 않습니다. agent CLI를 호출하기 전에 서버 실행 권한을 확인하고, 실제 등록 argv·환경으로 제한 시간 내 MCP `initialize` handshake를 수행합니다. 사전 점검이 실패하면 agent 설정은 변경되지 않습니다. agent CLI 자체가 실패하면 출력된 확인·rollback 안내를 따르며, 기존 동명 등록을 지울 수 있으므로 설치기가 임의로 remove하지 않습니다.

`--image`, `--cache-dir`, `--state-dir`, `--disk-size`, `--cpus`, `--memory-mib`, `--gvproxy`, `--server-command`로 등록 내용을 조정할 수 있습니다. 격리 없는 수명주기 테스트가 필요하면 `--backend process`를 명시합니다. 수동 rollback 명령은 다음과 같습니다.

```sh
codex mcp remove moraebox
claude mcp remove --scope user moraebox
```

서버는 실행 도구와 persistent Box 관리 도구를 제공합니다.

| 도구 | 용도 |
| --- | --- |
| `sandbox_exec` | 명령 하나를 실행하거나 비동기 세션 시작, 선택형 `box_id`로 파일 상태 재사용 |
| `sandbox_io` | 제한된 출력 읽기, stdin 쓰기·닫기, 크기 조절, 시그널 전송 |
| `sandbox_stop` | 세션을 중지하고 정리가 끝날 때까지 대기 |
| `sandbox_box_create` | OCI 이미지에서 persistent Box 생성 |
| `sandbox_box_list` / `sandbox_box_get` | persistent Box metadata 조회 |
| `sandbox_box_delete` / `sandbox_box_reset` | 명시적 확인 후 idle Box를 영구 변경 |
| `sandbox_box_clone` | 명시적 확인 후 독립된 durable Box 생성 |

MCP 스키마에서도 명령은 argv 배열입니다. 출력 청크는 에이전트가 바로 읽을 수 있는 UTF-8 평문으로 제공하며, 잘못된 UTF-8 바이트는 `U+FFFD`로 치환합니다. stdin 바이트는 계속 base64로 인코딩합니다.

## 동작 방식

```text
CLI / Rust SDK / MCP 서버
             │
      runtime supervisor
   lifecycle · deadline · I/O
             │
 image rootfs → immutable base ext4
       ├─ BoxId 없음: 실행별 CoW disk → 삭제
       └─ BoxId 있음: persistent disk → 유지
             │
    실행별 VMM helper 프로세스
       정식 libkrun ABI
             │
   console + vsock (TSI off)
 선택형 virtio-net ↔ gvproxy
             │
      일회용 Linux microVM
```

libkrun의 시작 작업은 context를 소비하고 호출 프로세스를 종료하므로 helper를 별도 프로세스 경계로 둡니다. helper를 CLI, SDK host, MCP 서버 밖에 두면 supervisor가 VM 소유권 handle 하나를 명확하게 관리할 수 있습니다.

모든 일회성 실행은 같은 상태 머신을 따릅니다.

```text
prepare → start → ready → running → stop → dead
   └──────── 실패 / 타임아웃 / 취소 ──────────────┘
```

`dead`는 helper 프로세스, control socket, I/O pump, 임시 파일, VM 리소스가 모두 회수됐다는 뜻입니다.

### 워크스페이스 구성

| Crate | 책임 |
| --- | --- |
| `moraebox-core` | 실행 명세, 수명주기 상태, 시그널, 제한된 출력 |
| `moraebox-box` | persistent Box metadata, lease, immutable base disk, ephemeral CoW disk |
| `moraebox-image` | OCI 레지스트리, digest 검증, 캐시, 워크스페이스 스냅샷 |
| `moraebox-runtime` | 백엔드, supervision, 세션, 진단, trace |
| `moraebox-sdk` | 비동기 임베딩 API |
| `moraebox-cli` | `morae` 명령줄 인터페이스 |
| `moraebox-mcp` | stdio MCP 서버와 에이전트 등록 |
| `moraebox-vmm-helper` | libkrun을 감싸는 서명된 네이티브 경계 |
| `moraebox-protocol` | 제한된 host/guest 프로토콜 타입 |

## 보안 모델

현재 위협 모델은 로컬 사용자가 자신의 macOS 호스트에서 실행하는 신뢰할 수 없는 Linux 게스트 코드입니다.

보안과 관련된 기본값은 다음과 같습니다.

- 실행에서 명시적으로 허용하지 않는 한 게스트 네트워크 인터페이스 없음
- Transparent Socket Impersonation 플래그를 0으로 설정한 control vsock
- 실행별 gvproxy virtio-net으로 egress를 허용하고 VM과 함께 정리
- 호스트 환경 전달 없음
- 암묵적인 셸 파싱 없음
- 불변 읽기 전용 워크스페이스 스냅샷
- traversal, device, unsafe link, symlink parent를 검사하는 digest 검증 OCI 콘텐츠
- 기본 1시간 deadline과 TERM 이후 KILL 승격
- 일회용 준비 unit과 부모 프로세스 종료 후 정리

Persistent Box는 filesystem 폐기의 명시적 예외일 뿐 VM 폐기의 예외가 아닙니다. 신뢰할 수 없는 guest 변경을 실행 사이에 보존할 수 있으므로 관련 작업에만 같은 Box를 사용하고 trust boundary를 넘기 전 delete 또는 reset해야 합니다. exclusive lease가 한 Box의 동시 writer를 막습니다.

moraebox는 적대적인 호스트 사용자, 손상된 hypervisor·VMM, 적대적인 멀티테넌트 환경으로부터 보호한다고 주장하지 않습니다. process 백엔드는 격리를 제공하지 않습니다.

## 플랫폼 지원과 현재 제약

| 영역 | 상태 |
| --- | --- |
| Apple Silicon macOS | 네이티브 libkrun 실행, 현재 릴리스 검증 대상 |
| Linux 및 Windows | 컴파일·테스트 대상, 네이티브 릴리스 런타임 없음 |
| libkrun 스택 | 정식 libkrun 1.19.4 및 libkrunfw 5.5.0으로 검증 |
| 이미지 소스 | 원격 OCI 레지스트리, 로컬 OCI layout과 Docker archive는 아직 가져오지 않음 |
| VM 재사용 | 구체화한 artifact는 캐시할 수 있지만 부팅한 신뢰 불가 VM은 재사용하지 않음 |
| Box 지속성 | opt-in 전체 root filesystem 지속성, 각 실행은 여전히 새 microVM 사용 |
| 워크스페이스 | 읽기 전용 스냅샷, 쓰기 overlay와 copy-out/diff는 후속 작업 |
| 대화형 I/O | PTY 지원, 실시간 크기 조절은 후속 작업 |

이 프로젝트는 아직 초기 단계입니다. 보안에 민감한 작업에 사용하기 전에 위 경계를 검토하세요.

## 소스에서 빌드

Rust 1.85 이상이 필요합니다.

```sh
cargo build --release --locked \
  -p moraebox-cli \
  -p moraebox-mcp \
  -p moraebox-vmm-helper

codesign --force --sign - \
  --entitlements assets/moraebox-vmm.entitlements \
  target/release/morae-vmm-helper
```

네이티브 실행에는 호환되는 정식 libkrun/libkrunfw 빌드, Hypervisor.framework, `e2fsprogs`의 `mke2fs`와 `e2fsck`, 선택형 네트워크 사용 시 `gvproxy`가 필요합니다. `morae doctor --json`은 기본 네이티브 준비 상태와 네트워크 준비 상태를 구분해 보고합니다.

## 개발

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

네이티브 macOS 변경에는 `morae doctor --json`과, 서명된 helper 및 네이티브 의존성을 사용할 수 있을 때 실제 백엔드 smoke suite도 필요합니다. CI는 macOS, Linux, Windows에서 이식 가능한 품질 게이트를 실행합니다.

Apple Silicon macOS에서는 이식 가능한 검사를 마친 뒤 서명된 네트워크 보안 게이트를 실행합니다.

```sh
scripts/native-egress-e2e.sh
```

이 게이트는 debug helper를 `assets/moraebox-vmm.entitlements`로 ad-hoc 서명하고, 준비된 기본 캐시 이미지를 사용해 network-off DNS/TCP/UDP 차단, network-on egress, timeout·취소·helper 실패 후 정리를 검증합니다. 다른 준비된 캐시 이미지는 `MORAE_NATIVE_E2E_IMAGE`, 기본값이 아닌 캐시는 `MORAE_NATIVE_E2E_CACHE_DIR`로 지정할 수 있습니다. 테스트 환경에서 외부 TCP/DNS 대상 변경이 필요하면 `MORAE_EGRESS_HOST`와 `MORAE_EGRESS_UDP_DNS`를 사용합니다.

버그 리포트와 범위가 명확한 pull request를 환영합니다. 네이티브 런타임 문제를 보고할 때는 백엔드, 호스트 플랫폼, 정확한 명령, `morae doctor --json` 출력을 포함해 주세요. 진단 정보를 공유하기 전에 로컬 경로나 민감한 값은 제거해야 합니다.

## 라이선스

moraebox는 [Apache License 2.0](LICENSE)으로 배포됩니다.
