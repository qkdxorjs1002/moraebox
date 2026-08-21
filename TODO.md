# moraebox TODO

현재 구현과 명시된 제품 의도를 대조해 남긴 작업 목록이다. 우선순위는 다음과 같다.

- **P0**: 수명주기·격리·데이터 무결성 또는 핵심 사용 흐름을 깨뜨릴 수 있어 우선 처리
- **P1**: 다음 릴리스 품질과 성능을 위해 처리
- **P2**: 핵심 계약을 유지하면서 기능 범위를 확장

## 유지해야 하는 제품 의도

아래 원칙은 개선 대상이 아니다. 관련 작업은 이 제약을 보존해야 한다.

- 실행 명령은 argv 배열이며 암시적 shell parsing을 추가하지 않는다.
- 게스트 네트워크와 호스트 환경 상속은 기본 비활성화로 유지한다.
- `process` backend는 테스트·개발용이며 VM 격리를 제공한다고 표현하지 않는다.
- 호스트 source directory를 직접 virtio-fs로 노출하지 않고 읽기 전용 snapshot 또는 bounded copy 경로를 사용한다.
- 실행된 untrusted VM은 lease 종료 후 폐기하며 pool로 되돌리지 않는다.
- persistent Box는 파일 상태만 유지하고 실행마다 새 microVM과 `SessionId`를 만든다.
- 기본 timeout은 1시간이며 unlimited는 `--timeout none` 또는 `--timeout 0`으로 명시한다.
- 기본 저장소는 사용자 전역 `~/.moraebox/{cache,state}`이고 기존 project-local 데이터는 자동 이동하지 않는다.
- MCP output은 agent가 바로 읽을 수 있는 lossy UTF-8 평문을 기본 계약으로 유지한다.
- 현재 보안 경계는 local single-user와 untrusted Linux guest이며 hostile host·multi-tenant·hypervisor compromise는 범위 밖이다.
- Apple Silicon macOS가 현재 native release target이다. Linux와 Windows native runtime은 이 목록의 릴리스 목표로 간주하지 않는다.
- `moraebox-protocol`과 single-use prepared pool은 제거 후보가 아니라 원래 설계에 맞게 통합할 구성 요소다.

## P0 — 정확성·정리·보안 불변식

### CLI interactive와 workspace

- [x] `morae run --tty --interactive`를 실제 양방향 스트리밍으로 구현한다.
  - 현재 stdin을 EOF까지 먼저 읽고 실행 종료 후 output을 재생하므로 prompt가 보이지 않고 입력에 반응하지 않는다.
  - stdin/stdout/stderr를 실행 중 동시에 pump하고 SIGINT·SIGTERM을 guest에 전달한다.
  - raw terminal 상태는 성공·오류·panic·signal 경로에서 모두 복구한다.
  - 재현:

    ```sh
    morae run --tty --interactive -- /bin/bash
    ls -la

    morae run --interactive -- /bin/bash
    ls -la
    ```

- [x] `--workspace` 실행이 `mke2fs` 단계에서 멈추는 현상을 재현하고 수정한다.
  - 단계별 timeout과 stderr diagnostics를 제공해 snapshot 순회, image 생성, `mke2fs`, attach 중 어디서 멈췄는지 구분한다.
  - source를 직접 공유하지 않고 immutable read-only ext4 snapshot 계약을 유지한다.
  - 실패·취소 시 `mke2fs`, helper, 임시 image를 모두 정리한다.
  - 재현:

    ```sh
    morae run --workspace ./ -- ls -la /workspace
    ```

### Runtime lifecycle

- [x] session command channel의 owner가 사라지면 busy-loop하지 말고 즉시 stop·cleanup으로 수렴한다.
  - `commands.recv() == None`을 owner-loss 이벤트로 취급한다.
  - child/helper, network proxy, I/O pump, 임시 disk와 socket이 모두 회수되는 회귀 테스트를 추가한다.

- [x] stdin write가 timeout, stop, signal, process exit 처리를 막지 않도록 전용 bounded writer task/queue로 분리한다.
  - 초기 stdin을 쓰기 전부터 wall-clock deadline을 적용한다.
  - guest가 stdin을 읽지 않는 상태에서도 timeout과 `sandbox_stop`이 완료되어야 한다.
  - queue의 byte/item 상한과 backpressure 동작을 명시한다.

- [x] 모든 backend 오류 경로에 teardown guard와 hard cleanup deadline을 적용한다.
  - TERM 실패 후에도 KILL/force-stop과 resource cleanup을 계속한다.
  - `force_stop`, exit wait, stdout/stderr pump 오류를 무시하지 않는다.
  - `dead`는 모든 host resource가 회수된 뒤에만 publish한다.

- [x] stop을 상태 기반으로 idempotent하게 만든다.
  - 반복 stop이 최초 kill deadline을 연장하지 않아야 한다.
  - 이미 stopping/dead인 session에는 안정적인 최종 상태를 반환한다.

- [x] SDK one-shot 실행에서 output buffer가 잘린 경우 `CursorExpired` 대신 보존된 output과 `truncated=true`, 정확한 cursor를 반환한다.
  - output limit보다 큰 stdout/stderr와 혼합 channel 회귀 테스트를 추가한다.

### MCP responsiveness와 resource ownership

- [x] MCP stdio server가 여러 request ID를 동시에 처리하도록 dispatcher와 단일 stdout writer를 분리한다.
  - 긴 `sandbox_exec(wait=true)` 중에도 `sandbox_io`, `sandbox_stop`, `ping`을 처리해야 한다.
  - response 순서는 달라도 ID 대응과 stdout protocol 무오염을 보장한다.

- [x] JSON-RPC cancellation과 client EOF를 session cleanup에 연결한다.
  - cancelled request와 연결 종료가 소유한 session을 정리한다.
  - 비동기 session을 의도적으로 유지할 수 있는 ownership 규칙을 명시한다.

- [x] SDK/MCP session registry에 active-session 상한, 완료 session TTL/reaper, 명시적 remove를 추가한다.
  - 완료된 session이 64 MiB output buffer와 함께 무기한 남지 않도록 한다.
  - 한도를 넘으면 안정적인 resource-limit 오류를 반환한다.

### OCI integrity와 host resource protection

- [x] digest-pinned image reference를 실제 manifest digest와 비교하고 기존 CAS object도 신뢰하기 전에 검증한다.
  - 손상된 기존 blob은 실패 또는 원자적 교체로 처리한다.
  - top-level manifest, selected platform manifest, 각 layer descriptor의 digest와 size를 모두 검증한다.

- [x] OCI download와 extraction에 compressed bytes, expanded bytes, layer/file count, per-file size, total disk quota를 적용한다.
  - descriptor size와 실제 body size 불일치를 거부한다.
  - free-space preflight와 decompression-bomb 회귀 테스트를 추가한다.

- [x] Bearer auth realm으로 registry credential을 전달하는 정책을 제한한다.
  - HTTPS를 강제하고 credential을 전달할 수 있는 registry/realm 관계를 검증한다.
  - 정상적인 cross-host token service는 지원하되 arbitrary realm에는 Basic credential을 보내지 않는다.

### Native network security gate

- [x] signed native stack에서 실제 egress E2E suite를 추가한다.
  - network-off: DNS와 TCP/UDP egress가 실패해야 한다.
  - network-on: DNS와 허용된 외부 접속이 성공해야 한다.
  - control vsock의 모든 TSI feature flag가 0인지 확인한다.
  - timeout, cancellation, helper crash 후 gvproxy와 socket이 남지 않아야 한다.

## P1 — 릴리스 품질과 성능

### Runtime와 API 일관성

- [x] `Supervisor`와 `SessionManager`의 실행·종료·I/O 로직을 하나의 lifecycle engine으로 통합한다.
  - CLI one-shot, SDK session, MCP가 `prepare → start → ready → running → stop → dead`를 공유한다.
  - process backend와 libkrun backend에 같은 timeout·termination semantics를 적용한다.

- [x] image pull, workspace 준비, base/ephemeral disk 준비, helper spawn을 전체 timeout budget에 포함한다.
  - 각 단계별 elapsed time과 실패 stage를 trace/report에 남긴다.

- [x] output pump 오류를 성공 결과로 숨기지 않고 typed I/O failure로 보고한다.

- [x] `RunSpec.inherit_env`의 process/libkrun 동작을 일치시킨다.
  - 기본 empty environment를 유지한다.
  - host environment 해석은 공통 계층에서 명시적으로 수행하고 non-Unicode 값의 정책을 정한다.

- [x] backend 이름 문자열 검사 대신 격리, TTY, network, Box, workspace 지원을 표현하는 typed capabilities를 도입한다.

- [x] output buffer가 대용량 read 중 pump를 막지 않도록 lock 범위와 복사를 줄이고 API별 최대 read 크기를 강제한다.

### MCP protocol와 usability

- [x] `sandbox_exec(wait=true)`의 inline output에 상한과 continuation cursor를 추가한다.
  - 큰 result를 `content.text`와 `structuredContent`에 두 번 복사하지 않는다.
  - 기본 output 형식은 현재의 lossy UTF-8 `text`를 유지한다.

- [x] MCP 입력을 server-side에서도 엄격히 검증한다.
  - unknown field, `max_bytes`, stdin decoded size, rows/columns pair, TTY dimensions를 검증한다.
  - `unlimited=true`와 `timeout_ms` 동시 지정은 ambiguity 오류로 처리한다.

- [x] stable tool error envelope를 정의한다.
  - `code`, `stage`, `retryable`, `message`, `remediation`을 제공한다.
  - cursor 만료에는 `earliest_cursor`를 포함한다.

- [x] `sandbox_wait` 또는 `sandbox_io.wait_ms`와 session list/status/remove 도구를 추가한다.
  - polling interval을 강제하지 않고 bounded long-poll을 제공한다.

- [x] initialize state와 지원 protocol version을 검증하고 `initialized`/cancel notification을 처리한다.

- [x] 각 tool에 `outputSchema`와 실제 side effect에 맞는 annotations를 추가한다.
  - persistent Box에서 실행하는 `sandbox_exec`와 signal/stop의 mutation을 숨기지 않는다.

- [x] MCP server 시작 시 default image를 eager pull하지 않고 protocol handshake를 먼저 받을 수 있게 한다.
  - 최초 실행의 lazy preparation 상태와 오류를 tool result로 보고한다.

- [x] MCP install 경로를 견고하게 만든다.
  - helper, libkrun, gvproxy, rootfs 등 지속되는 경로를 절대경로로 등록한다.
  - server executable permission과 initialize handshake를 사전 점검한다.
  - 실패 시 agent 설정을 남기지 않거나 명확한 rollback 안내를 제공한다.

### OCI/cache/workspace performance

- [x] cache 전체 배타 lock을 reference/digest/key 단위 lock으로 분리한다.
  - network download와 layer materialization은 global metadata lock 밖에서 수행한다.
  - 동일 digest publish는 짧은 atomic critical section에서 double-check한다.

- [x] registry response를 temp CAS file로 streaming하면서 hash와 size를 검증한다.
  - layer download는 bounded concurrency를 허용하되 적용 순서는 유지한다.
  - HTTP client/token을 재사용하고 connect/read/overall timeout, bounded retry, `Retry-After`를 적용한다.

- [x] layer extraction과 filesystem hashing을 `spawn_blocking`으로 분리해 async runtime을 막지 않는다.

- [x] CAS와 materialized image publish를 crash-safe하게 만든다.
  - unique `create_new` temp file/dir를 사용한다.
  - complete marker가 regular non-symlink file이고 예상 manifest digest를 담는지 확인한다.
  - atomic replace 후 parent directory를 fsync한다.

- [x] cache usage/list에서 rootfs를 매번 재귀 탐색하지 않도록 indexed metadata와 reconcile/repair를 제공한다.
  - logical size와 sparse/CoW physical allocation을 구분해 표시한다.

- [x] workspace snapshot의 source/image/source 반복 scan을 줄이면서 build 전후 source mutation 검증은 유지한다.
  - file/inode count를 ext4 sizing에 반영한다.
  - cache/state 경로가 source tree 내부에 있으면 재귀 포함 전에 거부한다.

- [x] image/cache/state root를 private permission으로 만들고 owner·symlink를 검증한다.

### CLI와 운영 경험

- [x] image pull, rootfs materialization, base disk, workspace snapshot, helper spawn 진행률을 stderr에 표시한다.
  - `--json` 또는 non-TTY에서는 protocol-friendly structured/quiet 동작을 사용한다.

- [x] 반복되는 storage/json/native path 옵션을 global options와 명확한 config precedence로 정리하고 shell completion을 제공한다.

- [x] project-local `.moraebox`가 발견되면 자동 이동 없이 기존 데이터 사용 방법만 경고한다.

- [x] Box list가 손상된 한 entry 때문에 전체 실패하지 않도록 healthy entry와 per-entry error를 함께 반환하고 repair/quarantine 명령을 제공한다.

- [ ] `--output-limit`과 `--kill-grace`를 CLI/MCP에서 명시적으로 설정할 수 있게 한다.

- [ ] `--json` 실행 오류에도 stable JSON error envelope와 stage를 제공한다.

- [ ] image pull policy `missing|always|never`를 추가하고 실제 resolved digest를 결과에 표시한다.

- [ ] `doctor`가 실제 cache volume의 reflink 지원, free space, network helper/socket, signing, ABI 상태를 각각 진단하고 remediation을 제공한다.

### Box와 native backend

- [ ] Box clone/reset과 ephemeral disk 준비에 reflink/clonefile 또는 sparse-aware copy를 사용한다.
  - CoW clone은 별도의 독립 disk identity를 유지한다.
  - 실제 cache volume에서 capability를 검사한다.

- [ ] base disk 준비 lock을 key별 waitable lock으로 바꾸고 동일 key만 직렬화한다.

- [ ] crash 후 남은 `.creating`, `.deleting`, 임시 disk와 tombstone을 age/lock 기준으로 안전하게 GC한다.

- [ ] writable Box 실행 전 `Dirty` 상태를 durable하게 기록하고 clean shutdown/e2fsck 이후에만 `Ready`로 전환한다.

- [ ] gvproxy readiness를 경로 존재가 아닌 socket handshake로 확인하고 bounded stderr diagnostics를 보존한다.

- [ ] root disk 준비가 끝난 뒤 network proxy를 시작하고 중간 실패에도 proxy process를 reap한다.

- [ ] helper/libkrun/libkrunfw의 architecture, executable permission, code signature, 실제 released ABI/version을 `doctor`와 spawn 전에 검증한다.

### 구조와 검증

- [ ] CLI/MCP/native에서 중복된 storage, image, disk-size, helper/tool discovery를 공통 builder/config 계층으로 옮긴다.

- [ ] 큰 CLI, MCP server, image cache, libkrun adapter를 command/transport/storage/backend 책임별 모듈로 분리한다.

- [ ] 문자열과 `Box<dyn Error>` 중심 오류를 stage와 retryability를 보존하는 typed error로 전환한다.

- [ ] 다음 회귀 테스트를 보강한다.
  - owner loss, blocked stdin, repeated stop, pump failure, output cap 초과
  - MCP concurrent IDs, cancellation, EOF, invalid schema, large output
  - mock registry digest/auth/timeout/retry/concurrent pull
  - Box/cache 중단 쓰기, stale staging, power-loss recovery

- [ ] CI에 Rust 1.85 MSRV와 `--locked` job을 추가하고 native signed smoke의 실행/skip 이유를 명시한다.

- [ ] dependency/license/advisory 검사와 GitHub Actions commit SHA pinning을 추가한다.

## P2 — 의도된 기능 확장

> 현재 순차 개선 작업 범위에서 제외한다. 미래 백로그로만 유지한다.

- [ ] `moraebox-protocol`을 bounded versioned vsock host/guest protocol에 실제로 연결한다.
  - exec, streaming I/O, signal, resize, copy-in/out frame과 size/path validation을 포함한다.
  - MCP stdout과 guest protocol diagnostics가 섞이지 않게 한다.

- [ ] single-use prepared pool을 실제 startup 경로에 통합하고 warm lease p50/p95/p99를 측정한다.
  - 아직 untrusted command를 실행하지 않은 prepared unit만 lease한다.
  - lease 반환 후 VM을 재사용하지 않고 파기·보충한다.
  - image pull, template build, workspace import 시간을 warm lease SLO와 분리한다.

- [ ] live PTY resize를 SIGWINCH부터 native controller까지 연결한다.

- [ ] read-only workspace 원본을 유지하면서 writable overlay와 bounded copy-out/diff를 제공한다.

- [ ] local OCI layout과 Docker archive import를 지원하고 remote registry와 동일한 digest/path 검증을 적용한다.

- [ ] Box에 label/tag, last-used, physical allocation, 정렬·필터와 rename을 제공한다.

- [ ] Box export/import/backup과 versioned metadata migration을 제공한다.
  - booted VM snapshot이나 untrusted VM 재사용 기능으로 구현하지 않는다.

- [ ] benchmark를 cold/warm startup, first output, full completion, concurrent throughput, peak RSS, cache hit으로 분리한다.
  - build/host/native dependency metadata와 오류 수를 결과에 포함한다.

- [ ] property/fuzz/loom/miri와 성능 회귀 기준을 안전성·동시성 핵심 경로에 단계적으로 추가한다.
