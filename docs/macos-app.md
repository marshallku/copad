# macOS App (copad-macos)

> ⚠️ **Doc currency note (2026-06-05)**: many sections below still
> describe the original SwiftTerm-backed implementation. Phase 10b
> (2026-06-05) removed the SwiftTerm path; the current renderer is
> `alacritty_terminal` + a custom AppKit/CoreText draw layer
> (`AlacrittyTerminalViewController`). The "Phase 6 / 7" sections at
> the bottom + [macos-post-renderer-catchup.md](./macos-post-renderer-catchup.md)
> are authoritative for the current state. Older sections kept as
> historical record + reference for SwiftTerm-era gotchas that may
> still inform decisions if a similar pattern arises.

## Overview

Swift/AppKit app. **Current renderer**: `alacritty_terminal` (Rust
staticlib via `copad-term/`) feeding a Metal painter (`MetalGridRenderer`
glyph-atlas pipeline on a CAMetalLayer-backed `AlacrittyRenderView`) —
**the default since the slice-3 flip** (~5.5× cheaper main-thread
render than CoreText). The original AppKit/CoreText draw layer stays
as the `[renderer] gpu = false` opt-out and the automatic
Metal-unavailable fallback — see
[macos-gpu-renderer-plan.md](./macos-gpu-renderer-plan.md).
**Historical**: started on [SwiftTerm](https://github.com/migueldeicaza/SwiftTerm)
(commit `e0ddf31` 2026-05-18 flipped default to alacritty; Phase 10b
2026-06-05 removed SwiftTerm entirely).

**Tech stack:** Swift 6, AppKit, macOS 14+, `libcopad_term.a` (Rust staticlib wrapping `alacritty_terminal`)

**Build system:** Swift Package Manager (standalone, not in Cargo workspace)

---

## Architecture

### Linux vs macOS 비교

| 항목 | Linux | macOS |
|---|---|---|
| UI framework | GTK4 | AppKit (NSWindow/NSViewController) |
| Terminal widget | VTE4 (`vte4::Terminal`) | SwiftTerm (`LocalProcessTerminalView`) |
| PTY 관리 | VTE 내장 (`spawn_async`) | SwiftTerm 내장 (`startProcess`) |
| IPC | D-Bus + Unix socket | Unix socket |
| 메인 스레드 전달 | `glib::timeout_add_local` 폴링 | `DispatchQueue.main.sync` (동기) |
| 설정 파싱 | `toml` crate | 직접 구현 (simple line parser) |
| 테마 | `copad-core/theme.rs` | `Theme.swift` (mirrors Rust struct) |
| 분할 창 | GTK Paned | `NSSplitView` + `EqualSplitView` |

### 디렉토리 구조

```
copad-macos/
├── Package.swift                      # SwiftTerm 의존성 선언 + 타깃 분리 (Copad / CopadCore / CopadCoreTests)
├── run.sh                             # .app 번들 생성 후 실행
├── Sources/CopadCore/                 # 순수-Swift 라이브러리 (실행파일과 분리)
│   └── ContextService.swift          # copad_core::context 미러 (PaneContext + ContextService)
├── Tests/CopadCoreTests/              # XCTest — Xcode 툴체인 필요 (CLT는 XCTest 미동봉)
│   └── ContextServiceTests.swift     # pane_context 페어리티 테스트
└── Sources/Copad/
    ├── CopadApp.swift                  # @main 진입점
    ├── AppDelegate.swift              # NSWindow 생성, 메뉴바, 소켓 커맨드 라우팅
    ├── TabViewController.swift        # 탭 목록 관리, PaneManager 배열
    ├── TabBarView.swift               # 커스텀 탭바 UI + 패널 추가 팝오버
    ├── PaneManager.swift              # 단일 탭의 split-pane 트리 관리
    ├── SplitNode.swift                # N-ary 분할 트리 데이터 구조
    ├── CopadPanel.swift                # 모든 패널 타입 공통 프로토콜
    ├── TerminalViewController.swift   # SwiftTerm 래퍼, 쉘 실행, delegate
    ├── WebViewController.swift        # WKWebView 래퍼, CopadPanel 구현
    ├── EventBus.swift                 # 이벤트 브로드캐스트 허브 + 채널
    ├── SocketServer.swift             # POSIX Unix socket 서버 (async handler)
    ├── Config.swift                   # config.toml 파서
    └── Theme.swift                    # 10개 내장 테마 (RGBColor, CopadTheme)
```

`CopadCore`는 Rust `copad_core` 미러를 위한 순수-Swift 라이브러리 타깃. GUI 레이어가 Xcode의 stricter Swift 6 concurrency check에서 막혀서 `swift test`로 빌드가 실패하는 문제를 우회하기 위해 실행파일 타깃과 분리했음. 새로 추가하는 순수-로직 미러(EventBus 형 코어 모델 등)는 여기에 두는 것이 권장. 실행파일 `Copad`는 `swiftLanguageMode(.v5)`로 핀되어 CLT (6.2.4) / Xcode (6.3.2) 두 컴파일러 모두에서 빌드됨.

---

## 빌드 및 실행

### Dev 루프 (debug bundle, 임시)

```bash
cd copad-macos
swift run                # 빠른 테스트 — 메뉴바 이름 등이 올바르지 않을 수 있음

./run.sh                 # 권장: .build/debug/Copad.app 새로 만들고 open으로 실행
swift build              # 빌드만
```

`run.sh`는 매번 `Copad.app/Contents/Info.plist`를 포함한 번들을 새로 만들어서 `open`으로 실행합니다. Info.plist가 있어야 Dock 아이콘, 메뉴바 앱 이름 등이 정상 표시됩니다.

### 영구 설치 (`/Applications`)

```bash
./scripts/install-macos.sh             # ~/Applications + ~/.cargo/bin/coctl (no sudo)
./scripts/install-macos.sh --system    # /Applications + ~/.cargo/bin/coctl (sudo for /Applications)
./scripts/install-macos.sh --launch    # 설치 후 바로 open
./scripts/install-macos.sh --no-build  # 이미 .build/release/Copad 있을 때
./scripts/install-macos.sh --no-coctl # coctl 재설치 스킵
```

설치 동작:
1. `cargo build --release -p copad-ffi` → `target/release/libcopad_ffi.a` (Rust staticlib for Swift FFI)
2. `swift build -c release` → `copad-macos/.build/release/Copad` (links libcopad_ffi.a)
3. `pkill -x Copad`로 실행 중 인스턴스 종료 (macOS는 실행 중 .app의 exec를 lock)
4. tmp 디렉토리에 `.app` staging → `mv`로 atomic 설치 (실패해도 dest가 깨지지 않음)
5. `cargo install --path copad-cli` → `~/.cargo/bin/coctl`

**왜 `cargo install`을 wrap하나:** `cargo install copad-cli` (crates.io 미배포)과 `cargo install --path .` (workspace 루트는 virtual manifest)는 모두 실패합니다. 올바른 형태는 `cargo install --path copad-cli`이고, 스크립트가 이걸 wrap합니다.

**Info.plist 중복:** `run.sh`와 `install-macos.sh`가 동일 Info.plist를 인라인으로 포함합니다. 두 카피는 의도된 상태 (Rule of Three: 2회는 OK). 세 번째가 생기면 그때 템플릿화.

### `copadd` 수동 실행 (PR 1 / daemon-first migration prep)

Linux는 daemon-first가 default(`83c5122`); macOS는 monolithic 상태에서 daemon migration 진행 중(`docs/macos-daemon-migration-plan.md`). PR 1에서는 `copadd`를 수동으로 띄워 coctl로 호출하는 흐름까지만 검증합니다 (Copad.app은 아직 daemon에 연결되지 않음).

```bash
# 1. daemon 빌드
cargo build --release -p copad-daemon
cargo build --release -p copad-cli   # coctl

# 2. daemon 띄우기 (foreground; 별도 터미널 권장)
target/release/copadd

# 3. 다른 터미널에서 coctl로 호출 — Copad.app이 떠 있으면 coctl 디스커버리가
#    GUI 소켓을 먼저 잡을 수 있어 모놀리식 GUI로 라우팅됩니다. PR 1에서는 daemon
#    경로를 검증하는 게 목적이므로 --socket 또는 COPAD_SOCKET으로 daemon 소켓을
#    명시합니다.
DAEMON_SOCK="$HOME/Library/Caches/copad/socket"

target/release/coctl --socket "$DAEMON_SOCK" call system.ping
# {"status": "ok"}

# `system.list_actions`는 macOS GUI의 in-process registry 전용 — daemon은
# 이 메서드를 register하지 않으므로 daemon socket으로 호출하면 unknown_method.
# Daemon-side plugin이 정상 spawn됐는지는 plugin 자체의 health-check action
# 으로 확인. 예: echo plugin은 ping을 노출.
target/release/coctl --socket "$DAEMON_SOCK" call echo.ping --params '{"hi":"there"}'
# {"echoed": {"hi": "there"}, "from": "copad-plugin-echo"}  ← daemon-side echo plugin 정상

# 4. 종료
pkill -x copadd
```

소켓 경로: `~/Library/Caches/copad/socket` (perms `0600`, `copad-core::paths::daemon_socket_path()` 결정). 디렉토리는 daemon이 첫 실행 시 생성. 종료 후 socket inode가 남지만 다음 daemon 시작 시 unbind/rebind 됩니다.

PR 2에서 `Copad.app`이 정식 daemon client가 되면 GUI 소켓도 daemon으로 forward되므로 `--socket` 명시 없이 동일 동작이 보장됩니다. PR 1 단계에선 두 소켓이 별도 프로세스를 가리키므로 명시가 필수.

이후 PR(2~)에서 `Copad.app`이 `~/Library/Caches/copad/socket`을 향해 자동 connect + auto-spawn하도록 확장합니다 (`docs/macos-daemon-migration-plan.md` 참조).

### Rust FFI (`copad-ffi` + `CCopadFFI`)

PR 1 / Tier 2.1 spike. macOS 앱이 shared Rust core(`copad-core`의 trigger engine, supervisor 등)를 향후 사용하려면 Swift ↔ Rust C-ABI 다리가 필요합니다. SwiftPM은 cargo를 prebuild로 직접 호출할 방법이 없어서 빌드 파이프라인이 두 단계로 갈라집니다:

```
cargo build --release -p copad-ffi   →  target/release/libcopad_ffi.a
swift build                          →  Copad (links libcopad_ffi.a)
```

`run.sh` / `install-macos.sh` 둘 다 두 단계를 묶어 줍니다. `swift build`만 단독으로 돌리면 link 단계에서 undefined-symbol 에러가 납니다 (clean target/ 상태에서).

**구성:**
- `copad-ffi/` — workspace member, `crate-type = ["staticlib"]`. `serde_json`만 의존. 4개의 `extern "C"` 심볼 노출:
  - `copad_ffi_version()` — 정적 C 문자열, 해제 불필요
  - `copad_ffi_call_json(input)` — JSON in / JSON out, 결과는 heap-alloc (caller가 free)
  - `copad_ffi_free_string(*mut c_char)` — `call_json` 결과 해제
  - `copad_ffi_last_error()` — thread-local 마지막 에러 메시지 (borrowed)
- `copad-macos/Sources/CCopadFFI/` — SwiftPM C target. `include/copad_ffi.h` (수동 declaration, `copad-ffi/src/lib.rs`와 동기화) + `include/module.modulemap` (`module CCopadFFI { ... }`) + `dummy.c` (SwiftPM이 header-only target은 link graph에 안 넣어서 placeholder C 파일 필요).
- `copad-macos/Package.swift` — `Copad` exec target에 `linkerSettings: [.unsafeFlags(["-L../target/release"]), .linkedLibrary("copad_ffi")]`. 상대경로 `../target/release`는 link 시점에 패키지 루트(`copad-macos/`) 기준으로 풀려 cargo workspace target 디렉토리를 가리킴.
- `copad-macos/Sources/Copad/FFIBridge.swift` — `CopadFFI` enum 파사드. C 포인터를 Swift 호출자에게 노출하지 않음. `String(cString:)` copy 후 즉시 `copad_ffi_free_string`으로 free, ownership round-trip 닫힘.

**Smoke test:** `AppDelegate.applicationDidFinishLaunching`의 `CopadFFI.runSmokeTest()`가 매 launch마다 stderr에 `[copad-ffi] version = ...` + `[copad-ffi] echo round-trip = ...`를 찍음. `echoed_at`는 Rust가 만든 unix-ms timestamp이므로 round-trip이 진짜로 일어나는지 확인 가능. PR 5c가 실제 trigger engine startup도 같은 staticlib로 추가했음 (아래 Trigger Engine 섹션 참고).

### Trigger Engine via FFI (PR 5c / Tier 2.4)

`copad_core::trigger::TriggerEngine`을 그대로 wrap. Engine 자체는 Rust에 있고(Linux/macOS 공통 단일 진실), macOS는 C-ABI 다리만 소유. await preflight + completion fan-out + `covering_patterns` dedup + reload reconcile 같은 미묘한 시맨틱은 모두 Rust 코어가 처리.

**Layered chain:**
```
echo plugin → event.publish (stdio)
   ↓
PluginSupervisor reader thread → eventBus.broadcast(event:, data:)
   ↓
EventBus.onBroadcast hook → CopadEngine.dispatchEvent(kind:, payload:)
   ↓
copad_engine_dispatch_event (FFI) → Rust TriggerEngine.dispatch
   ↓ (per matched trigger)
FfiSink.dispatch_action → C action callback
   ↓
Swift @convention(c) trampoline → Task { @MainActor in }
   ↓
ActionRegistry.tryDispatch(action, params, completion)
   ↓
matched handler (system.ffi_test, plugin action, etc.) → completion
```

**FFI surface (5 신규 심볼, `copad-ffi/src/lib.rs`):**
- `copad_engine_create()` → `*mut EngineHandle`
- `copad_engine_destroy(*mut)` 
- `copad_engine_set_action_callback(handle, fn_ptr, user_data)`
- `copad_engine_set_triggers(handle, triggers_json)` — JSON array of trigger objects, returns count or -1
- `copad_engine_dispatch_event(handle, kind, payload_json)` — returns # triggers fired
- `copad_engine_count_triggers(handle)` — diagnostic

**Swift wrapper (`CopadEngine` in `FFIBridge.swift`):**
`@unchecked Sendable` (NOT `@MainActor`) — Rust engine 자체가 thread-safe (`RwLock` 내부)이라 main actor pinning 불필요. `dispatchEvent`가 plugin reader thread에서 직접 호출됨. C action callback 안에서만 `Task { @MainActor in }`으로 hop해서 ActionRegistry 접근. lifetime은 `Unmanaged.passRetained(self)`로 engine이 self를 retain → `shutdown()`이 명시적으로 release.

**Trigger 정의 (config.toml):**
```toml
[[triggers]]
name = "heartbeat-to-ffi-demo"
action = "system.ffi_test"
params = { source = "trigger", note = "fired by heartbeat" }

[triggers.when]
event_kind = "system.heartbeat"
```

`Config.swift`가 raw TOMLKit `TOMLTable`로 walk해서 `[[String: Any]]`로 추출 (specific Swift Decodable 타입 안 만듦 — params/payload_match가 임의 nested 값이라 Rust serde Deserialize에 위임). `setTriggers`로 engine에 push, hot-reload 시 재적용.

**Critical bug (검증 중 발견):**
초기엔 `CopadEngine`을 `@MainActor`로 마킹했음. 이러면 모든 plugin 이벤트가 main actor를 거쳐야 해서 reader thread → main hop이 매 이벤트마다 발생. 더 큰 문제: 이미 main이 다른 작업 중이면 trigger dispatch가 backlog. 수정: `@unchecked Sendable` + `nonisolated(unsafe)` for `actionRegistry` slot. Rust 엔진 lock이 동시성 처리, Swift는 callback 시점에만 main hop.

**Deferred (Rust 엔진은 지원하지만 spike 데모에 없음):**
- await/preflight chains (`[triggers.await]` 절)
- `<action>.completed` / `<action>.failed` fan-out (Phase 14.1)
- `covering_patterns` dedup (overlapping globs)
- 진짜 lazy `onAction:<glob>` activation (현재는 eager spawn + log)

**Universal binary:** 현재 spike는 cargo 호스트 아키텍처(arm64) only. x86_64 사용자가 생기면 `cargo build --target aarch64-apple-darwin && cargo build --target x86_64-apple-darwin && lipo -create … -output target/release/libcopad_ffi.a` recipe로 합치면 됨 (PR 1 시점에 deferred).

### ActionRegistry completion fan-out (PR 7)

PR 5c 의 chain 다이어그램은 **upstream** event (plugin → bus → engine → action) 만 그렸음. PR 7 은 거기에 **downstream** completion event 를 더한다 — registered action 이 끝나면 `<method>.completed` (또는 `RPCError` 일 때 `<method>.failed`) 가 같은 bus 로 broadcast 돼서 chain 의 다음 단계가 발화 가능. `examples/triggers/vision-flow-3.toml` 의 `git.worktree_add.completed → claude.start` 같은 chain 을 macOS 에서도 그대로 표현 가능해짐.

**Chokepoint:** Linux 와 동일하게 `ActionRegistry.tryDispatch` 안에서 broadcast (Linux `with_completion_bus(bus)` invariant). Plugin RPC reply 도 `PluginSupervisor` 가 `proc.invoke(..., completion: completion)` 을 registry handler 안에서 호출하기 때문에 자동으로 wrapping path 통과 → 별도 supervisor-side broadcast 불필요 (double-fire 방지).

**Wrapping path:**
```
coctl call kb.read --params '{...not-found...}'
    ↓
SocketServer → AppDelegate.handleCommand
    ↓
ActionRegistry.tryDispatch("kb.read", params, completion=replyToSocket)
    ↓ wraps:
let wrapped = { value in
    if !silent { eventBus.broadcast("kb.read.failed" or "kb.read.completed") }
    completion(value)  // original — replies to socket client
}
    ↓
PluginSupervisor's registered handler → proc.invoke(...) → stdio reply
    ↓ (reader thread)
wrapped(RPCError(code: "not_found", ...))
    ↓
broadcast("kb.read.failed", {code, message}) FIRST
    ↓
EventBus.onBroadcast hook → CopadEngine.dispatchEvent
    ↓ (chain continues, e.g. trigger fires alert action)
THEN completion(value) — coctl client receives error envelope
```

**Payload normalization:** EventBus 의 `[String: Any]` 계약을 유지하기 위해 success result 가 dict 면 그대로, 아니면 `["value": result]` 로 wrap. `RPCError` 는 `["code": err.code, "message": err.message]` 형태. 결과적으로 trigger interpolation 입장에선 `{event.X}` 로 모든 케이스 접근 가능 (단, dict 가 아닌 경우는 `{event.value}` 로 한 단계 들어가야 함).

**registerSilent (silent flag):** Linux 의 `register_silent` mirror. introspection action (`system.ping`, `system.list_actions`, future `context.snapshot`) 처럼 polling tooling 이 자주 부르는 action 은 silent 로 등록해서 bus flood 막음. 현재 `system.ffi_test` (debug round-trip) + `system.list_actions` (introspection) 가 silent. plugin action 들과 향후 builtin (`claude.start`, `webview.*`) 은 noisy default.

**Linux invariant 유지:** registered action 만 fan out. legacy switch-arm dispatch (`AppDelegate.handleCommand` 의 `webview.*`/`terminal.*`/`tab.*` 등) 는 registry 우회 → completion event 미발행. 이걸 registry 로 마이그레이션하면 chain surface 가 넓어짐 — 별개 parity 작업.

**Source-stamp trust boundary (cross-review CRITICAL):** Linux 의 `try_promote_or_drop_preflight` (`copad-core/src/trigger.rs:482`) 가 await-chain promotion 에 source check 를 검: `event.source == COMPLETION_EVENT_SOURCE` (`"copad.action"`) 일 때만 await 상태 머신을 advance. 안 그러면 plugin 이 임의로 emit 한 `<X>.completed` 이벤트가 await chain 을 silently advance 시킬 수 있음. macOS FFI 가 원래 `Event::new(kind, "macos.eventbus", payload)` 로 모든 이벤트 source 를 hardcode 했었기 때문에, registry-synth 된 completion 도 trust check 를 통과 못 해서 `[triggers.await]` chain 이 silently 통째로 stall 됐을 거. 

Fix: `copad_engine_dispatch_event` C signature 에 `source` 인자 추가, Swift 쪽 모든 hop 에 propagate — `EventBus.broadcast(event:source:data:)` (default `"macos.eventbus"` back-compat) → `EventBus.onBroadcast` 3-tuple closure → `CopadEngine.dispatchEvent(kind:source:payload:)` → FFI 로 source 그대로 전달. FFI 안에서 `let source_str = if source.is_null() { "macos.eventbus" } else { CStr::from_ptr(source).to_string_lossy() }` 로 default 처리. `ActionRegistry.publishCompletion` 만 `bus.broadcast(event:..., source: "copad.action", data:...)` 로 호출. 다른 broadcast 사이트는 unchanged.

`ActionRegistry.completionEventSource = "copad.action"` 라는 Swift-side 상수가 Rust 의 `COMPLETION_EVENT_SOURCE` 와 sync 돼야 함 (hand-maintained — 향후 Linux 가 상수 바꾸면 macOS 도 수동 업데이트).

**Codex pressure-test 결과 (Round 1+2 + cross-review):** Round 1 에서 plan 에 있던 4개 우선순위 중 3개 빗나감을 잡음 — 제일 큰 finding 은 vision-flow-3 의 `git.worktree_add.completed` 가 Phase 17 file-watcher event 와 다른 메커니즘 (registry-emitted action-completion 이벤트) 이라 macOS ActionRegistry 의 completion fan-out 이 진짜 Vision Flow 3 critical path 라는 거. Round 2 에서 3개 architecture 안 (A: FFI-side fan-out, B: registry chokepoint, C: hybrid) 비교해서 B 채택 — `PluginSupervisor.swift:105` 가 plugin action 을 registry 에 등록하니 plugin RPC reply 도 자동으로 `tryDispatch` chokepoint 통과, double-fire 없음. 그 후 cross-review 에서 source-stamp 누락을 CRITICAL 로 잡아서 위 fix 추가.

**End-to-end 검증:**
1. `~/.config/copad/config.toml` 에 `[[triggers]] when.event_kind = "echo.ping.completed", action = "system.list_actions"` + 동일 형태의 `kb.read.failed` 트리거 추가
2. `coctl call echo.ping --params '{"hi":"x"}'` → stderr `[copad-engine] event echo.ping.completed fired 1 trigger(s)`
3. `coctl call kb.read --params '{"id":"nope"}'` → stderr `[copad-engine] event kb.read.failed fired 1 trigger(s)`
4. Source-plumbing fix 적용 후에도 두 chain 모두 정상 발화 (regression 없음). Await-promotion path 는 코드 inspection 으로 검증 — Swift `publishCompletion` 의 source 가 FFI 까지 그대로 전달되어 `Event::new(kind, source_str, payload)` 시점에 `"copad.action"` 으로 stamp 되고 trigger.rs:482 의 trust check 를 통과. 라이브 `[triggers.await]` chain test 는 별도 PR (Vision Flow 3 자체가 await 안 쓰고, await 는 Phase 14.2 chained workflow 용으로 예약).

### `claude.start` (PR 8 / Phase 18)

Linux 의 `socket.rs::handle_claude_start` 1:1 포팅. Vision Flow 3 (Todo start → Slack 컨텍스트 → Jira → worktree → tmux + Claude REPL with pre-seeded prompt) 의 macOS 마지막 한 조각. PR 7 의 completion fan-out 과 결합돼서, `todo.start_requested → git.worktree_add → git.worktree_add.completed → claude.start` 의 chain 이 macOS 에서도 fully 작동.

**구조:**
```
coctl call claude.start --params {workspace_path, [session_name], [resume_session], [prompt]}
    ↓
ClaudeStart.dispatch (@MainActor) → param validation + canonicalize
    ↓
deriveSessionName / sanitizeSessionName / validateTmuxSessionName (Linux 1:1)
    ↓
shellSingleQuote(sessionName) + shellSingleQuote(claudeCmd)
    ↓
TabViewController.newTerminalTab(cwd:, initialInput:)
    ↓
PaneManager(initialPanel: .terminalSeed(cwd:, initialInput:))
    ↓
TerminalViewController(cwd:, initialInput:)
    ↓
SwiftTerm startProcess(currentDirectory: cwd) + tv.send(txt: initialInput)
    ↓ (background thread)
spawnPromptSeeder(sessionName, prompt)
    ↓ (poll tmux capture-pane for marker, max 8s)
    ↓ (cross-check tmux display-message #{pane_current_command} == claude|node)
    ↓ tmux load-buffer + paste-buffer + send-keys Enter Enter
```

**Race-safety:** SwiftTerm 의 `startProcess` 은 PTY 를 먼저 열고 child fork → exec 순서. master fd 에 `tv.send(txt:)` 로 쓴 바이트는 kernel buffer 에 머물다가 child shell 이 read 할 때 소비됨. Linux 의 VTE `spawn_async` callback 트릭이 macOS 에선 불필요.

**Prompt seeder 의 두 readiness gate:**
1. `tmux capture-pane -p -t <session>` 출력에 claude-specific marker (`Anthropic`, `Try "`, `claude --`, "claude code") 존재
2. `tmux display-message -p -t <session> '#{pane_current_command}'` 결과가 `claude` 또는 `node`

둘 중 하나라도 실패하면 stderr 에 `[claude.start] refusing to paste prompt into session ...` 로그 + return. `claude.start` 는 이미 success reply 한 상태라 caller 에 영향 없음. Shell-injection 가드 — `tmux new-session -A` 가 동명의 기존 session 에 attach 만 할 때, 그 session 이 shell 이면 prompt 가 raw shell command 로 실행될 위험을 차단.

**End-to-end 검증:**
- 5 개 validation path 전부 정확한 RPCError 반환 (missing workspace_path / 존재하지 않는 path / file-not-dir / prompt+resume mutual exclusion / `-flag-like` session_name)
- happy path: `coctl call claude.start --params '{"workspace_path":"/tmp/copad-claude-test","prompt":"..."}'` → 새 tab spawned, tmux session created, claude 부팅, seeder 가 prompt paste, claude 응답 확인됨
- refuse-to-paste guard: `tmux new-session -d -s shell-only "sleep 60"` 으로 미리 생성한 shell session 대상 `claude.start` → seeder 가 `saw_claude_marker=false, pane_current_command="sleep"` 감지 후 paste 거부, sleep pane 손상 없음

**Deferred:** macOS 에서 `vision-flow-3.toml` 의 trigger chain 을 e2e 로 dry-run 한 적은 없음 — Phase 14.2 chained workflow 작업 때 같이 검증 예정. tmux 는 런타임 의존 (`/usr/bin/env tmux` 로 shell-out), 번들링 안 함 (Linux 도 동일 posture).

### Plugin Supervisor (`PluginManifest` + `PluginSupervisor`)

PR 3 / Tier 3 spike. Linux의 `service_supervisor.rs` (1794 LOC)를 native Swift로 최소 surface만 포팅. echo plugin 하나 굴리는 데 필요한 것만 들어감.

**Discovery:**
`PluginManifestStore.discover()`가 두 디렉토리를 union:
1. `~/Library/Application Support/copad/plugins/<name>/plugin.toml` (macOS-native, 우선)
2. `~/.config/copad/plugins/<name>/plugin.toml` (XDG fallback — Linux/macOS dotfile-sharing 사용자용)

같은 plugin name이 둘 다 있으면 macOS path가 이김. parse 실패한 manifest는 stderr에 로그 + skip.

**TOMLKit Decodable gotcha:**
TOMLKit의 Decoder는 `var foo: T = default` 디폴트 값을 무시함. Swift init feature지 Decodable feature가 아님. 누락된 키는 `keyNotFound` throw. serde의 `#[serde(default)]`처럼 동작하려면 `init(from:)`에서 `decodeIfPresent ?? <default>` 명시적으로. PR 3에서 echo manifest 디코딩 시 이 버그로 한 번 실패해서 잡았음.

**Spawn + init handshake:**
service.activation == "onStartup"인 service만 spawn (PR 5에서 `onAction:*` / `onEvent:*` 추가). `Process` + 3 `Pipe` (stdin/stdout/stderr), `executableURL`은 `<plugin_dir>/<service.exec>` 우선 → 없으면 `$PATH` 직접 lookup (`/usr/bin/which` shell-out 안 함 — `$PATH` 재평가가 process spawn 시점에 다시 일어나서). reader thread 먼저 띄우고 `{id, method: "initialize", params: {protocol_version: 1}}` 보낸 뒤 5s 시한으로 `InitBox.semaphore.wait()`. 응답 받으면 manifest의 provides[] subset인지 검증 (런타임이 manifest보다 더 많이 claim하면 reject — 다른 plugin 액션 shadow 위험), `ActionRegistry.register(name, ...)`로 등록, `{method: "initialized"}` notification 발사.

**Action dispatch:**
ActionRegistry handler는 `proc.invoke(action:params:completion:)` 호출. UUID 만들어 `pending[id] = completion` 저장 (NSLock-protected), `{id, method: "action.invoke", params: {name, params}}` stdin 전송, 즉시 리턴. reader thread가 응답에서 id 매칭 → completion(payload) 호출. payload는 `decodeResponse(obj)`가 `result` 또는 `RPCError`로 변환.

**Critical bug (verification 중 발견):**
초기 구현은 reader thread completion을 `DispatchQueue.main.async`로 main actor에 hop했음. 그런데 init handshake 중 main thread가 `initBox.semaphore.wait()` 으로 park돼 있어서 main에 hop된 작업이 영원히 안 깨어남 → deadlock. **수정:** completion은 reader thread에서 inline 호출. SocketServer leaf와 InitBox.resolve 둘 다 actor isolation 가정 없는 leaf (sema signal + class var set)이라 thread-safe. main actor 작업이 필요한 미래 completion은 자기 body에서 hop하면 됨.

**Shutdown:**
`applicationWillTerminate`에서 `pluginSupervisor.shutdown()`. 각 process에 `{method: "shutdown"}` 보내고 200ms 대기, 살아있으면 `process.terminate()`. pending request는 모두 `RPCError(plugin_shutdown)`로 reject. SIGTERM/SIGKILL은 applicationWillTerminate를 안 거치지만, 어차피 parent가 죽으면 plugin의 stdin이 EOF되면서 reader loop 종료 → `main()` return → 자연사.

**의도적으로 deferred (PR 4+):**
- restart-on-crash policy
- `event.publish` notification → EventBus 포워딩 (PR 5에서 trigger engine과 함께)
- `event.dispatch` (host → plugin) — echo는 subscribe 안 함
- provides[] cross-plugin 충돌 해결
- Process group (`setpgid(0,0)`) + PID file 기반 crash recovery (codex I5)
- 코드 서명 — 로컬 빌드는 quarantine xattr 없어서 Gatekeeper 패스, release tarball에서는 별도 처리 필요

**Install flow:**
`scripts/install-macos.sh` 가 `MACOS_PLUGINS` 배열 (`echo git llm calendar kb todo bookmark slack discord` — 9 first-party) 돌면서 각각 `cargo build --release -p copad-plugin-<name>` → `~/Library/Application Support/copad/plugins/<name>/`에 manifest copy + binary copy (symlink 아님 — `git clean target/`로 silently 깨지지 않게). `--no-plugins` 플래그로 스킵 가능.

**End-to-end 검증:**
```bash
coctl call system.list_actions
# {"count":3, "names":["echo.ping","system.ffi_test","system.list_actions"]}

coctl call echo.ping --params '{"hello":"x","sleep_ms":200}'
# {"echoed":{"hello":"x","sleep_ms":200}, "from":"copad-plugin-echo"}  # 200ms+ blocking
```

**Activation 처리 (PR 4 확장):**
- `onStartup` → eager spawn (PR 3 원래 동작)
- `onAction:<glob>` → eager spawn + 로그 (`lazy not yet implemented, spawning eagerly`). 진짜 lazy는 placeholder handler 등록 → first call에서 spawn → 후속 호출 큐잉 패턴이 필요해서 PR 5 (트리거 엔진)와 같이 갈 예정. git처럼 spawn cost < 100ms인 plugin은 eager가 손해 거의 없음. Slack/Calendar처럼 startup이 무거운 (auth, network) plugin들이 들어오기 시작하면 진짜 lazy 구현이 의미를 가짐.
- `onEvent:<glob>` → 로그 후 skip. event-driven plugin을 eager로 띄우면 자원만 낭비 — 매칭되는 이벤트가 와야만 의미가 있고, 이벤트 라우팅은 트리거 엔진이 한다.

**MACOS_PLUGINS (현재 install-macos.sh에서 자동 빌드/설치):**
- `echo` (PR 3) — 프로토콜 sanity check
- `git` (PR 4) — 6개 액션 (`git.list_workspaces`/`list_worktrees`/`worktree_add`/`worktree_remove`/`current_branch`/`status`). cross-platform deps만 사용 (serde, serde_json, toml). `~/.config/copad/workspaces.toml` (또는 `COPAD_GIT_WORKSPACES_FILE` env override)에서 워크스페이스 정의 읽음. config 없으면 list_workspaces가 빈 배열 반환 (`fatal_error: null`) — graceful.
- `llm` (PR 5a) — Anthropic provider. 3개 액션 (`llm.complete`/`llm.usage`/`llm.auth_status`). `keyring` crate `apple-native` feature 통해 macOS Keychain에 토큰 저장. spike target으로 추가됨 — 자세한 건 아래 Keychain 섹션.
- `calendar` (PR 5b) — Google Calendar (read-only). 3개 액션 (`calendar.list_events`/`calendar.event_details`/`calendar.auth_status`). `onStartup` polling daemon: 백그라운드 thread가 `COPAD_CALENDAR_POLL_SECS` 간격으로 Google API polling, `COPAD_CALENDAR_LEAD_MINUTES` 시점에 `calendar.event_imminent` 이벤트 publish. credential 없으면 RPC 액션은 `not_authenticated` 반환하고 poller는 idle 대기 — `Config::minimal()` fallback 덕에 supervisor handshake는 통과. polling-daemon supervisor lifecycle 검증 차원에서 추가됨.
- `kb` / `todo` / `bookmark` (PR 6) — 셋 다 atomic-create-or-fail 의 rename primitive에 의존했는데, 원래 inline `libc::renameat2(RENAME_NOREPLACE)` 호출이라 Linux-only `compile_error!` 게이트 가 걸려있었음. PR 6 에서 `copad_core::fs_atomic::rename_no_replace` 로 추출 — Linux 는 그대로 `renameat2`, macOS 는 `renamex_np(RENAME_EXCL)` 매핑 (semantically identical: target 존재시 `EEXIST`). 게이트는 `any(linux, macos)` 로 완화됨. `O_NOFOLLOW` / `OsStrExt` 는 원래부터 macOS 에서도 OK (`std::os::unix::ffi`, `bsd/mod.rs::O_NOFOLLOW`). End-to-end: `kb.search` fulltext hit, `todo.create` → `todo.list` round-trip via `renamex_np`, `bookmark.add` 의 `existed: false` → `existed: true` no-replace dedup 모두 검증.
- `slack` / `discord` (PR 6) — `unix` cfg gate라 컴파일은 원래부터 OK였고, `keyring` 코드 경로가 llm/calendar와 동일 (`store_kind: "keyring"` 검증됨). PR 6 에서 `MACOS_PLUGINS` 에 추가. RPC 호출은 token 없으면 `not_authenticated` 반환 — fully functional 하려면 plugin별 `auth` 서브커맨드로 Slack Socket Mode token (xoxb- bot + xapp- app-level) / Discord Gateway bot token 을 사용자가 한 번 등록해야 함.

### Keychain / `keyring` 통합 (PR 5a 발견)

`slack/calendar/llm/discord` 모두 `keyring = { features = ["apple-native", "sync-secret-service", "crypto-rust"] }` 사용. macOS에선 `apple-native` feature가 Apple Security framework의 `SecItem*` API를 호출해서 user keychain에 entry를 만든다 (`SERVICE`/`ACCOUNT` 컴포지트 키).

**spike 결과 (`copad-plugin-llm` 통해 검증):**
1. `apple-native` 컴파일 OK — 모든 4개 plugin이 macOS host에서 cargo build 통과
2. runtime에 Keychain backend 바인딩 OK — plugin spawn 후 `auth_status` 호출 시 `store_kind: "keyring"` 반환. 만약 keychain이 못 잡혔으면 plaintext file fallback이 떴을 텐데(`store_kind: "file"`) 그게 아님
3. `keyring::Error::NoEntry` (entry 없을 때) graceful — `credentials_source: "none"` + `authenticated: false`로 변환되어 caller가 깔끔하게 처리 가능. crash나 fatal_error 안 남
4. plugin stderr에 `[llm] token store: keyring (env key: empty — falls back to store)` 로그 — backend 선택 가시화

**아직 미검증 (write path):**
write는 `keyring::Entry::set_password` 호출. macOS는 새 application이 keychain에 처음 쓸 때 user prompt를 띄울 수 있음 (signed app은 자동 허용, unsigned binary는 매 launch마다 prompt 가능). spike에서는 read만 했고 write는 actual ANTHROPIC_API_KEY가 필요해서 보류 — slack/calendar/discord auth flow 들어올 때 같이 검증 예정.

**auth flow (out-of-band):**
plugin은 두 가지 모드로 실행:
- supervisor RPC mode (default, no args) — copad이 spawn해서 stdio로 통신
- `auth` subcommand — user가 직접 실행: `ANTHROPIC_API_KEY=sk-ant-... copad-plugin-llm auth`. API call로 키 validate 후 keychain에 `{api_key, validated_at}` 저장. 이 시점에 macOS Keychain prompt가 뜰 수 있음. 한 번 저장되면 후속 copad launch에서 supervisor가 같은 binary path 사용하므로 keychain ACL이 그대로 유효 (signed/unsigned 무관).

binary path: `~/Library/Application Support/copad/plugins/llm/copad-plugin-llm`. install-macos.sh가 매 install마다 같은 path에 copy하므로 keychain ACL re-prompt 안 일어남 (path가 ACL 키의 일부).

**Calendar는 OAuth device-code flow 사용** (`copad-plugin-calendar auth`):
1. user가 Google Cloud Console에서 OAuth 2.0 client (Desktop type) 만들고 Calendar API 활성화 → `client_id` + `client_secret` 받음
2. `COPAD_CALENDAR_CLIENT_ID=<id> COPAD_CALENDAR_CLIENT_SECRET=<secret> copad-plugin-calendar auth` 실행
3. plugin이 stderr에 `user_code`와 verification URL 출력, Google 토큰 endpoint를 polling
4. user가 브라우저에서 코드 입력 + 권한 승인
5. plugin이 token + refresh_token 받아서 keychain에 `{access_token, refresh_token, ...}` 저장 (이때 macOS Keychain prompt 발생 가능)
6. 후속 copad launch 시 supervisor가 spawn한 calendar 프로세스가 keychain에서 token 읽어서 polling 시작

Slack/Discord도 동일하게 별도 `auth` subcommand로 토큰 setup (Slack은 Socket Mode token, Discord는 Bot Token). 셋 다 copad GUI launch와 무관한 out-of-band 한 번-실행 setup.

---

## 파일별 구현 세부사항

### CopadApp.swift

`@main`으로 진입점 선언. `NSApplication.shared`를 직접 다루어 `AppDelegate`를 설정하고 `app.run()`으로 이벤트 루프 시작.

### AppDelegate.swift (`@MainActor`)

- `CopadConfig.load()` → `CopadTheme.byName()` 순서로 설정·테마 로드
- `NSWindow` 생성 (1200×800, titled/closable/resizable/miniaturizable)
- `TabViewController`를 `window.contentViewController`로 설정
- 메뉴바: App(종료), Shell(탭/분할/전환), Find(검색), View(줌) 구성
- 소켓 서버 시작 후 `handleCommand(method:params:)`로 모든 커맨드 라우팅

**Find 메뉴 (in-terminal search):**
`performFindPanelAction(_:)` 메서드를 AppDelegate에 직접 선언하고, 활성 터미널 뷰에 동일한 셀렉터로 포워딩합니다. SwiftTerm의 `MacTerminalView`가 `performFindPanelAction(_:)`을 구현하고 있어서 Cmd+F / Cmd+G / Cmd+Shift+G가 SwiftTerm 내장 검색바를 트리거합니다. 검색바에는 case-sensitive, regex, whole-word 옵션이 포함됩니다.

**ActionRegistry seam (PR 2 / Tier 2.3):**
`handleCommand(method:params:completion:)`의 첫 줄이 `actionRegistry.tryDispatch(method, params:, completion:)`를 호출. 등록된 핸들러가 있으면 거기서 처리하고 리턴; 없으면 (`tryDispatch` returns `false`) 기존 hardcoded switch로 fall-through. PR 3(플러그인 슈퍼바이저)와 PR 5(트리거 엔진)가 이 registry에 자기 핸들러를 등록할 예정.

`registerBuiltinActions()`에서 launch 시점에 `system.*` 액션 등록:
- **`system.ffi_test`** — `params`(없으면 `{caller: "system.ffi_test"}`)를 `CopadFFI.callJSON`으로 통과시키고 `{echoed, ffi_version}` 반환. socket → registry → FFI → Rust → 다시 socket까지 entire seam을 단발 호출로 검증 가능. PR 5가 트리거 엔진 dispatch target 으로도 재사용 예정.
- **`system.list_actions`** — `{count, names: [...]}` 반환. 디버깅 + 추후 플러그인 등록 확인용.

```bash
coctl call system.list_actions
# {"count": 2, "names": ["system.ffi_test", "system.list_actions"]}

coctl call system.ffi_test --params '{"hello":"x"}'
# {"echoed": {"echoed_at": <ms>, "hello": "x"}, "ffi_version": "copad-ffi 0.1.0"}
```

`ActionRegistry`는 Linux full surface(`register_silent` / `register_blocking` / `invoke` / `try_invoke` / completion bus)의 일부만 가져옴. `register_blocking`은 의도적으로 보류 — Linux는 dispatch 즉시 리턴 후 worker thread에서 blocking 핸들러 실행하지만, macOS 소켓 dispatch가 main-actor completion 까지 socket thread를 세마포어로 잡아두는 모델이라 섞으면 데드락 위험 있음. PR 5에서 트리거 엔진 wiring할 때 async boundary 다시 설계.

**Command Palette (PR 6 / daemon-migration)** — Cmd+Shift+P로 `NSWindow` sheet 열기. `CommandPaletteController`가 `NSSearchField` + `NSTableView`를 들고 있으며, AppKit이 data source/delegate를 unowned로 잡으므로 `AppDelegate.commandPaletteController` ivar에서 sheet lifetime 내내 retain. shortcut match는 `event.keyCode == 0x23` (`kVK_ANSI_P`) — non-Latin IME(Korean/JP)가 char `p`를 자기 문자로 번역해서 char-match가 깨지는 거 회피. 액션 surface = `actionRegistry.names()` ∪ `CommandPalette.macOSLegacyMethods` (handleCommand switch arms 미러 — daemon쪽 `LEGACY_DISPATCH_METHODS`와 다름: macOS는 `tab.switch`, `pane.focus_*`, `terminal.shell_*`, `webview.state` 추가 보유). Enter는 `handleCommand`로 라우팅해서 registry → legacy switch → daemon fallback 순으로 적중. `tab.close`는 `NSAlert` Cancel-default + Cancel-on-stray-Enter (Linux `command_palette` decision 29 미러). post-dispatch focus restore는 안 함 — `tab.close`/`tab.new`/`split.*`가 active view를 변형하므로 stale responder 복원이 crash/conflict.

### TabViewController.swift (`@MainActor`)

탭 목록을 `[PaneManager]`로 관리. `contentArea`에 현재 탭의 `containerView`를 embed합니다.

**주요 흐름:**
```
newTab() / newWebViewTab(url:)
  └─ PaneManager 생성 → onLastPaneClosed / onActivePaneChanged 연결
  └─ NotificationCenter로 terminalTitleChanged 구독
  └─ eventBus 전파
  └─ tab.opened 이벤트 발행
  └─ switchTab(to: last)

switchTab(to:)
  └─ 이전 containerView removeFromSuperview
  └─ 새 containerView를 contentArea에 fill constraints로 embed
  └─ layoutSubtreeIfNeeded() → startIfNeeded() → makeFirstResponder()
```

**주요 프로퍼티:**
- `activeTerminal: TerminalViewController?` — 활성 터미널 패널
- `activeWebView: WebViewController?` — 활성 웹뷰 패널
- `eventBus: EventBus?` — didSet 시 모든 PaneManager에 전파

**소켓 커맨드 메서드:** `tabList()`, `tabInfo()`, `renameTab(at:title:)`, `sessionList()`, `sessionInfo(index:)`, `execCommand(_:)`, `feedText(_:)`, `terminalState()`, `readScreen()`

### TabBarView.swift

NSView 기반 커스텀 탭바. CopadTheme 색상을 사용합니다.

- 각 탭: 제목 label + × 버튼, hover 시 배경색 전환
- 오른쪽 끝: + 버튼 → `AddPanelPopoverController` 팝오버 표시
- `onSelectTab`, `onCloseTab`, `onNewPanel` 클로저로 이벤트 전달
- `TabBarView.height = 36` (상수)

**`AddPanelPopoverController`:**
Terminal / Browser 행 × Tab / Split Horizontal / Split Vertical 버튼(SF Symbol 아이콘). `onNewPanel: ((AddPanelType, AddPanelMode) -> Void)?` 클로저로 결과 전달. Linux의 GTK Popover와 동일한 UX.

### PaneManager.swift (`@MainActor`)

단일 탭의 분할 창 트리를 관리합니다.

**핵심 설계:**
- `containerView` (NSView): TabViewController가 한 번만 embed, 이후 내부만 rebuild
- `root: SplitNode`: 현재 분할 상태의 N-ary 트리
- `activePane: any CopadPanel`: 현재 포커스된 패널 (터미널 또는 웹뷰)
- `eventBus: EventBus?`: didSet 시 모든 패널에 `assignEventBus(to:)` 전파
- split/close 때마다 `rebuildViewHierarchy()` 호출 → fresh `EqualSplitView` 트리 생성

**`EqualSplitView` (private):**
`NSSplitView` + `NSSplitViewDelegate`를 결합한 내부 클래스.
`splitView(_:resizeSubviewsWithOldSize:)` delegate에서 첫 번째 호출 시 모든 subview를 균등 분배하고 `initialSizeSet = true`로 잠금. 이후 호출은 `adjustSubviews()`(기본 동작)에 위임해 사용자 드래그가 동작합니다.

`layout()`이 아닌 delegate를 쓰는 이유: NSSplitView는 `resizeSubviews`(→ delegate)로 subview frame을 확정한 뒤 `layout()`을 호출합니다. `layout()` 시점에는 이미 잘못된 frame이 커밋된 상태이므로 `setPosition`을 거기서 불러도 신뢰할 수 없습니다. delegate를 사용하면 NSSplitView가 "지금 subview frame 정해"라고 위임하는 바로 그 순간에 개입할 수 있습니다.

**포커스 감지:**
SwiftTerm의 `MacTerminalView.becomeFirstResponder`는 `public`이지만 `open`이 아니라서 외부 모듈에서 override 불가. `NSEvent.addLocalMonitorForEvents(matching: .leftMouseDown)`으로 클릭 위치를 확인해 포커스 전환합니다.

**터미널 종료 연결:**
`wirePanel(_:)`에서 `TerminalViewController`로 캐스트해 `onProcessTerminated` 클로저 등록. 활성 pane이면 `closeActive()`, 비활성이면 트리에서 제거 후 rebuild.

### SplitNode.swift

N-ary 재귀 트리 enum:

```swift
indirect enum SplitNode {
    case leaf(any CopadPanel)                    // 터미널 또는 웹뷰
    case branch(SplitOrientation, [SplitNode])  // N개 자식 가능
}
```

동일성 비교는 `===` 대신 `ObjectIdentifier`를 사용합니다 (`any CopadPanel`은 `Equatable`이 아님).

**`splitting(_:with:orientation:)` — 항상 계층형 분할:**
focused leaf를 항상 새 2-child branch로 교체합니다. 같은 방향의 부모 branch에 flat하게 추가하지 않습니다.

```
[A] → split →
  branch(H, [leaf(A), leaf(B)])
  A=50%, B=50%

focus A → split →
  branch(H, [branch(H, [leaf(A), leaf(C)]), leaf(B)])
  A=25%, C=25%, B=50%  ← B 크기 변화 없음
```

**`removing(_:)` — collapse:**
제거 후 자식이 1개만 남으면 branch를 collapse해 단일 leaf로 승격.

### TerminalViewController.swift (`@MainActor`)

**`CopadTerminalView` (private subclass):**
두 가지 역할의 래퍼:
1. SwiftTerm의 `processTerminated` 미호출 버그 우회 — `installExitMonitor()`에서 별도 `DispatchSource.makeProcessSource`를 설치.
2. OSC 52 clipboard write를 정책으로 게이트 — `installDelegateProxy(policy:)`에서 `CopadTerminalDelegate` proxy를 SwiftTerm의 `terminalDelegate` slot에 install. SwiftTerm의 `clipboardCopy`가 `public`(non-`open`)이라 직접 override 불가하기 때문에 delegate slot 자체를 가로챔. 자세한 배경은 troubleshooting.md 참조.

**`CopadTerminalDelegate` (private proxy):**
`TerminalViewDelegate`를 구현. host(`LocalProcessTerminalView`)에 weak ref. `sizeChanged` / `setTerminalTitle` / `hostCurrentDirectoryUpdate` / `send` / `scrolled` / `rangeChanged`은 host의 `public` 구현에 forward (PTY winsize 갱신, 타이틀 업데이트, OSC 7, 키 입력 등 정상 동작). `clipboardCopy`만 `OSC52Policy`로 분기 — `.deny`(기본)는 stderr 한 줄 로그 + drop, `.allow`는 SwiftTerm 원래 동작 (`NSPasteboard.general`에 write). `requestOpenLink` / `bell` / `iTermContent`는 protocol extension의 default를 그대로 둠.

정책은 `[security] osc52 = "deny" | "allow"` (config), hot-reload는 `applyOSC52Policy(_:)` → `delegateProxy?.policy = ...`로 전달.

**`startIfNeeded()`:**
Shell은 뷰가 계층에 추가되고 `layoutSubtreeIfNeeded()` 이후에만 시작. frame 없이 `startProcess`를 호출하면 SwiftTerm이 행/열 수를 0으로 계산합니다.

**환경변수:**
```swift
var env = ProcessInfo.processInfo.environment.map { "\($0.key)=\($0.value)" }
env.append("TERM=xterm-256color")
env.append("COLORTERM=truecolor")
env.append("COPAD_SOCKET=/tmp/copad-\(pid).sock")
```
`startProcess(environment:)`에 배열을 넘기면 부모 환경이 완전히 교체되므로 반드시 현재 환경을 상속해야 합니다.

**소켓 커맨드 메서드:**
- `execCommand(_:)` — 명령어 + 줄바꿈 PTY 전송
- `feedText(_:)` — 원시 텍스트 PTY 전송
- `terminalState()` — cols/rows/cursor/title
- `readScreen()` — 현재 화면 텍스트 + 커서 위치
- `history(lines:)` — 스크롤백 N줄 (SwiftTerm 음수 row 인덱스 활용)
- `context(historyLines:)` — state + screen + history 합산
- `setCustomTitle(_:)` — 커스텀 탭 제목 (자동 업데이트 억제)

**`setTerminalTitle` delegate:**
```swift
nonisolated func setTerminalTitle(...) {
    Task { @MainActor in
        guard self.customTitle == nil else { return }  // 커스텀 제목이 있으면 무시
        self.currentTitle = title.isEmpty ? "Terminal" : title
        ...
        eventBus?.broadcast(event: "panel.title_changed", ...)
    }
}
```

**`hostCurrentDirectoryUpdate` delegate:**
OSC 7 URI(`file://hostname/path`)에서 `URL(string:).path`로 hostname을 제거해 `/Users/...` 형태의 순수 경로만 추출 후 `terminal.cwd_changed` 이벤트 발행.

**`processTerminated` delegate:**
`panel.exited` 이벤트 발행 후 `onProcessTerminated` 클로저 호출.

### CopadPanel.swift

모든 패널 타입의 공통 인터페이스:

```swift
@MainActor
protocol CopadPanel: AnyObject {
    var view: NSView { get }
    var currentTitle: String { get }
    func startIfNeeded()
    func applyBackground(path: String, tint: Double)
    func clearBackground()
    func setTint(_ alpha: Double)
    func removeFromParent()
}
```

`TerminalViewController`와 `WebViewController` 모두 이 프로토콜을 구현. `SplitNode`와 `PaneManager`는 `any CopadPanel`로 동작해서 터미널과 웹뷰를 같은 분할 트리에 섞어 쓸 수 있음.

### WebViewController.swift (`@MainActor`)

`WKWebView` 래퍼. `CopadPanel` 프로토콜 구현.

- `startIfNeeded()` — 초기 URL 로드 (없으면 blank page)
- `navigate(to:)` — URL 탐색 (scheme 없으면 `https://` 자동 추가)
- `goBack()` / `goForward()` / `reload()` — 네비게이션
- `executeJS(_:completion:)` — JS 평가 (비동기, WKWebView 콜백)
- `getContent(completion:)` — `document.documentElement.outerHTML` 반환
- `toggleDevTools()` — `developerExtrasEnabled` 토글 (Safari Web Inspector)
- `WKNavigationDelegate.webView(_:didFinish:)` — 탭 제목 업데이트 → `terminalTitleChanged` 알림 발행

**배경 이미지:** `applyBackground`, `clearBackground`, `setTint` 모두 no-op (WebView는 자체 렌더링).

### EventBus.swift

이벤트 브로드캐스트 허브. Linux의 `Arc<Mutex<Vec<Sender<String>>>>`와 동일한 역할.

```swift
final class EventBus: @unchecked Sendable {
    func subscribe() -> EventChannel       // 구독자 채널 반환
    func broadcast(event: String, data: [String: Any] = [:])
}

final class EventChannel: @unchecked Sendable {
    func send(_ event: String) -> Bool     // false = 채널 닫힘
    func receive() -> String?              // 이벤트 또는 close까지 블록
    func close()
}
```

`EventChannel`은 `NSLock` + `DispatchSemaphore`로 thread-safe 버퍼드 FIFO 구현. `SocketServer`의 `streamEvents(fd:)`가 `channel.receive()`에서 블록하며 이벤트를 클라이언트로 스트리밍.

**이벤트 형식:** `{"event":"<type>","data":{...}}\n`

| 이벤트 | data | 발생 시점 |
|--------|------|-----------|
| `panel.focused` | `{panel_id}` | 클릭으로 pane 포커스 |
| `panel.title_changed` | `{panel_id, title}` | 터미널/웹뷰 제목 변경 |
| `panel.exited` | `{panel_id}` | 쉘 프로세스 종료 |
| `tab.opened` | `{index, panel_id}` | 탭 생성 |
| `tab.closed` | `{index}` | 탭 닫힘 |
| `terminal.cwd_changed` | `{panel_id, cwd}` | OSC 7 CWD 변경 |
| `terminal.shell_precmd` | `{panel_id}` | 쉘 프롬프트 준비 |
| `terminal.shell_preexec` | `{panel_id}` | 명령 실행 직전 |
| `webview.loaded` | `{panel_id}` | 페이지 로드 완료 |
| `webview.title_changed` | `{panel_id, title}` | 웹뷰 제목 변경 |
| `webview.navigated` | `{panel_id, url}` | 웹뷰 URL 변경 |

### SocketServer.swift (비동기 핸들러)

```swift
var commandHandler: ((_ method: String, _ params: [String: Any], _ completion: @escaping (Any?) -> Void) -> Void)?
```

기존 동기 패턴에서 completion 기반으로 변경. 소켓 스레드는 `DispatchSemaphore`로 블록하고, 메인 스레드에서 핸들러가 completion을 호출하면 unblock. `webview.execute_js`처럼 WKWebView 콜백 이후에 응답해야 하는 커맨드를 지원.

**`ResultBox`**: `@unchecked Sendable` 클래스로 메인 액터 → 소켓 스레드 간 결과 전달. 세마포어가 직렬화를 보장하므로 안전.

### Config.swift

`~/.config/copad/config.toml`을 `LebJe/TOMLKit` (SwiftPM dep, 0.6.0)으로 디코드. `CopadConfig.parse`가 `TOMLDecoder().decode(RawConfig.self, …)`로 private shadow 타입에 매핑하고, 누락된 섹션·키는 모두 optional이라 `?? defaults.X` fallback로 처리.

**TOML 구조 (현재 macOS가 디코드하는 키만):**
```toml
[terminal]
shell = "/bin/zsh"
font_family = "JetBrains Mono"
font_size = 13
option_as_alt = true               # Option+printable → ESC + char (tmux/zsh Meta)
force_meta_keys = ["Return"]       # control-char keys that get ESC prefix too

[theme]
name = "catppuccin-mocha"

[background]
path = "~/Pictures/wall.png"      # alias: image
tint = 0.6
opacity = 0.95

[security]
osc52 = "deny"                    # or "allow"
```

**`option_as_alt` vs. `force_meta_keys`**: `option_as_alt = true`(default)는 Option+printable 키(예: Option+1, Option+a)를 `ESC + base_char`로 PTY에 보내서 tmux/zsh/readline Meta 바인딩이 동작하게 합니다. `option_as_alt`는 의도적으로 control-character 키(Return, Escape 등)를 제외합니다 — Cocoa의 `moveWordLeft:` / `deleteWordBackward:` 키 바인딩이 Option+←/Option+⌫을 readline 바이트로 변환하는 path를 보존해야 하기 때문.

`force_meta_keys`(default `["Return"]`)는 그 필터를 명시적으로 우회할 키 화이트리스트입니다 — Opt+Return이 `ESC + CR`을 보내서 Claude Code / Python REPL / ipython 등의 newline-in-prompt가 동작합니다. 두 옵션은 직교: `option_as_alt = false`(악센트 입력자)여도 `force_meta_keys = ["Return"]`만 켜면 Claude Code newline은 살아 있습니다.

지원 키 (case-insensitive): `Return`/`Enter`, `Escape`/`Esc`. 알 수 없는 이름은 stderr 경고 후 무시. Arrows/Delete는 지원하지 않습니다 (Cocoa 바인딩 경로 보존 필요). 끄려면 `force_meta_keys = []`.

Linux 쪽 schema의 `[tabs]`, `[statusbar]`, `[keybindings]`, `[[triggers]]`는 macOS가 아직 안 읽지만 TOMLKit이 unknown key를 silently 무시하므로 Linux 사용자가 같은 config 파일을 그대로 쓸 수 있음. malformed TOML은 stderr에 `[copad] config.toml parse failed: …`를 찍고 defaults로 fallback (crash 안 함).

**snake_case 매핑:** TOMLKit 0.6에는 `keyDecodingStrategy`가 없어서 `font_family`, `font_size`만 `TerminalSection.CodingKeys`에서 명시적으로 매핑. 단일 단어 키는 그대로 둠.

**기본 폰트:** `"JetBrains Mono"` — macOS에 기본 설치된 NSFont family 이름. Nerd Font 변형(`JetBrainsMono Nerd Font Mono`)은 별도 설치 필요하므로 기본값으로 쓰지 않습니다.

### Theme.swift

`RGBColor(hex:)` — hex 문자열 → `(r, g, b: UInt8)` 변환.

SwiftTerm에 넘길 때 8비트 → 16비트 변환:
```swift
SwiftTerm.Color(red: UInt16(c.r) * 257, green: UInt16(c.g) * 257, blue: UInt16(c.b) * 257)
```
`* 257`(= `0x101`)로 곱하면 0 → 0, 255 → 65535로 정확히 매핑됩니다.

---

## 소켓 IPC

**아키텍처:**
```
coctl ──Unix socket──► SocketServer (background thread)
                                │
                    DispatchQueue.main.sync
                                │
                       AppDelegate.handleCommand()
                                │
                    TabViewController / TerminalViewController
                                │
                       Response → socket thread → coctl
```

프로토콜: copad-core `protocol.rs`와 동일한 newline-delimited JSON.

**지원 커맨드:**

| 커맨드 | 파라미터 | 동작 |
|---|---|---|
| `system.ping` | — | `{"status":"ok"}` |
| `event.subscribe` | — | 이벤트 스트림 구독 (long-lived 연결) |
| `terminal.exec` | `command` | 명령어 + 줄바꿈 PTY 전송 |
| `terminal.feed` | `text` | 원시 텍스트 PTY 전송 |
| `terminal.state` | — | cols/rows/cursor/title |
| `terminal.read` | — | 현재 화면 텍스트 + 커서 |
| `terminal.history` | `lines` (기본 100) | 스크롤백 텍스트 |
| `terminal.context` | `history_lines` (기본 50) | state + screen + history |
| `terminal.shell_precmd` | `panel_id` (선택) | shell_precmd 이벤트 발행 |
| `terminal.shell_preexec` | `panel_id` (선택) | shell_preexec 이벤트 발행 |
| `tab.new` | — | 새 터미널 탭 생성 |
| `tab.close` | — | 활성 pane 닫기 |
| `tab.switch` | `index` | 탭 전환 |
| `tab.list` | — | 탭 목록 |
| `tab.info` | — | 탭 목록 + pane 수 |
| `tab.rename` | `index`, `title` | 탭 이름 변경 (자동 제목 억제) |
| `split.horizontal` | — | 터미널 좌우 분할 |
| `split.vertical` | — | 터미널 상하 분할 |
| `session.list` | — | `tab.list`와 동일 |
| `session.info` | `index` | 특정 탭의 상세 정보 |
| `background.set` | `path`, `tint` (기본 0.6) | 배경 이미지 설정 |
| `background.set_tint` | `tint` | 틴트 알파값 변경 |
| `background.clear` | — | 배경 이미지 제거 |
| `agent.approve` | `message`, `title?`, `actions?` | NSAlert 모달 표시, 비동기 응답 |
| `webview.open` | `url?`, `mode?` (tab/split_h/split_v) | 웹뷰 패널 생성 |
| `webview.navigate` | `url`, `id?` | 웹뷰 URL 이동 (id 없으면 active fallback) |
| `webview.back` | `id?` | 뒤로 |
| `webview.forward` | `id?` | 앞으로 |
| `webview.reload` | `id?` | 새로고침 |
| `webview.execute_js` | `code`, `id?` (alias: `script`) | JS 평가 (비동기 응답) |
| `webview.get_content` | `id?` | 페이지 HTML 반환 (비동기) |
| `webview.devtools` | `id?`, `action?` (show/close/attach/detach/toggle) | Safari Web Inspector 토글 (close는 no-op) |
| `webview.state` | `id?` | url/title/can_go_back/forward/is_loading |

**`id` 파라미터**: 모든 `webview.*` 커맨드는 `params["id"]` (UUID)로 특정 패널을 지정할 수 있고, 생략하면 active webview로 fallback (Linux는 필수, macOS는 lenient default — Tier 1.6 plan 결정). `id`로 못 찾으면 `not_found`, 패널이 webview가 아니면 `wrong_panel_type` 에러 envelope 반환. `webview.execute_js`의 param 이름은 Linux/copad-cli convention인 `code`로 통일했고, 기존 macOS-only 호출자를 위해 `script`도 인식.

**에러 envelope**: 웹뷰 핸들러가 `RPCError(code:message:)`를 completion에 전달하면 `SocketServer.dispatch`가 감지해서 JSON-RPC 에러 형태(`{ok:false, error:{code, message}}`)로 wrap. Linux 코드와 일치 (`not_found`, `wrong_panel_type`, `invalid_params`, `no_active_webview`). 다른 핸들러는 기존 `(Any?) -> Void` completion을 그대로 사용.

---

## Linux 대비 미구현 기능 (포팅 예정)

### ~~Phase 2: WebView Panel~~ ✅ 구현 완료

`WebViewController.swift` + `CopadPanel.swift` 참조.

### ~~Phase 3: AI Agent & Shell Integration~~ ✅ 구현 완료

`EventBus.swift`, `SocketServer.swift` 참조. SwiftTerm path에서 미구현이던 항목들은 Phase 10a (alacritty 백엔드 default flip, 커밋 `e0ddf31`) 이후로 다시 가능해짐 — wiring은 별도 백로그:
- ~~**`terminal.output` 이벤트**~~ ✅ alacritty path에서 wiring 완료. `AlacrittyTerminalViewController`의 `sendInput` 헬퍼가 모든 키보드/페이스트 경로를 감싸서 `terminal.output` 발사. Linux `connect_commit` 동일 시맨틱 (마우스 휠 포워딩은 제외 — VTE도 제외).
- **OSC 9/777 `terminal.notification` 이벤트** — alacritty path에서는 `copad-term`이 OSC 파싱을 직접 하므로 OSC dispatch 분기에 한 줄 추가하면 됨.
- **Shell integration via OSC 133** — 위와 같은 이유로 alacritty path에서는 가능. 현재는 여전히 소켓 커맨드 (`terminal.shell_precmd` / `terminal.shell_preexec`)로 우회 중.

### Phase 4: Tab Bar & UX Polish
- ~~**Tab bar toggle** — 아이콘만 보이는 collapsed 모드 (Cmd+Shift+B), `tabs.toggle_bar` 소켓~~ ✅ 구현 완료
- ~~**Double-click rename** — 탭 라벨 더블클릭으로 인라인 편집~~ ✅ 구현 완료
- **Pane focus navigation** — 키보드로 다음/이전 pane 포커스 이동
- **Background random rotation** — `background.next` 소켓 커맨드, `[background] directory` 설정
- ~~**Config hot-reload** — 파일 변경 감지 후 테마/설정 즉시 반영~~ ✅ 구현 완료

### Phase 5: Distribution & Ecosystem
- **Session persistence / restore** — Linux landed commit `8a1312a` (`copad-linux/src/session.rs`, auto-save tabs/splits/cwd, auto-restore on launch). macOS 포트 대기 — 백로그: [macos-post-renderer-catchup.md §B](./macos-post-renderer-catchup.md).
- ~~Clipboard integration (OSC 52)~~ ✅ deny-by-default 구현 (`[security] osc52`); Linux 측 VTE는 이미 deny 기본
- ~~URL detection + click-to-open~~ ✅ OSC 8 hyperlink + 평문 URL Cmd+click (Phase 4b, 커밋 `6605b92`)
- ~~Plugin system~~ ✅ HTML/JS 패널 (`WKScriptMessageHandlerWithReply` 브리지) + daemon-routed 플러그인 호스트 (Swift `PluginSupervisor`는 macOS daemon-migration PR 5, 커밋 `2913441`에서 삭제됨 — daemon이 supervisor 소유)
- ~~Status bar~~ ✅ Waybar-style 3-zone + `[[modules]]` (Tier 4.2)

### Phase 6: Renderer migration (SwiftTerm → alacritty_terminal) ✅ Phase 10a + 10b
[macos-renderer-migration-plan.md](./macos-renderer-migration-plan.md) 완료. Phase 10a (`e0ddf31`)에서 default backend가 `alacritty`로 flip, Phase 10b (2026-06-05)에서 SwiftTerm path 완전 제거. `[renderer] backend = "swiftterm"` 설정은 silent-ignore (stale config 호환).

### Phase 7: Post-renderer catch-up
[macos-post-renderer-catchup.md](./macos-post-renderer-catchup.md) — renderer polish + Linux-parity 잔여. 현재 active 백로그.

---

## 알려진 주의사항

- **`NSSplitView` subview layout**: NSSplitView의 직접 자식은 `translatesAutoresizingMaskIntoConstraints = true` + `autoresizingMask = [.width, .height]`를 써야 합니다. Auto Layout을 사용하면 NSSplitView와 충돌합니다.
- **`[terminal] close_on_exit`**: shell이 exit (Ctrl+D, `exit`, parent killed)하면 pane을 자동으로 닫고 → 마지막 pane이면 탭 → 마지막 탭이면 윈도우까지 cascading close. 기본 `true` (Linux의 오래된 동작과 동일). `false`로 두면 `panel.exited`만 broadcast하고 dead-PTY viewport를 그대로 두므로 사용자가 exit 메시지를 읽고 Cmd+W로 직접 닫을 수 있음. macOS 경로: `AlacrittyRenderView.drainChildExit` → `onChildExited` 콜백 → `PaneManager.closePanel` → `TabViewController.onLastPaneClosed` → `closeTab(at:)`. Linux는 `tabs.rs::handle_panel_exit`에서 broadcast 후 같은 config 키로 게이트.
