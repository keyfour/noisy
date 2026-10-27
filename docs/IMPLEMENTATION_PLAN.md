# noisy Implementation Plan

**Status:** Active plan.
**Sources of truth:** `docs/SPECIFICATION.md` (0.3.0-draft), `docs/LUA_API.md`,
`docs/MODULE_GUIDE.md`. This plan adds delivery order, milestones, and
acceptance criteria; the specification owns the architecture.

---

## 1. What we are building

A single-process Qt alternative for non-GUI software, per the specification:

- Qt-style signals/slots: `emit` / `on` / `off` plus directed method calls,
  connected at runtime from Lua, Rust, or C++ with no code generation.
- Lua as the extension layer (the QML analog): `app.lua`, hot reload, no
  build step.
- Rust **and** C++ as first-class module and application languages:
  in-tree Rust modules via a trait, dynamic C++/Rust modules via one
  version-gated C ABI, both described by a runtime descriptor from which all
  Lua bindings are generated.
- One event bus as the only cross-language mechanism; JSON at every language
  boundary, typed structures kept inside each language.

The delivery measure of success: the framework examples
(`serial_to_net`, `web_monitor`, `data_pipeline`, `hello_service`,
`cpp_module`, `cpp_app`) run end-to-end in CI with no hardware and no
external network.

---

## 2. Architecture anchors (from the specification — restated for traceability)

| Anchor | Where in the design |
|---|---|
| Descriptor-driven modules | spec §3.3 — Lua namespaces generated from descriptors; module authors write no binding code |
| The two module paths | spec §3.4 — in-tree Rust trait + dynamic C ABI `.so` (Rust cdylib or C++), same JSON vocabulary |
| Lua dispatch thread | spec §3.1 — one `mlua::Lua` owned by a dedicated thread; commands and events cross via a job inbox; no re-entrancy |
| Actor threads for dynamic modules | spec §6.2 — one thread per module, one message at a time, `poll`-based fd watch, no locks, no spawns |
| Callback tokens | spec §3.1 — Lua closures cross into modules as opaque tokens, invoked via `invoke_callback` |
| Cross-module call rule | spec §6.1 — synchronous `call` is rejected on re-entrant recursion; `call_async` for mutual recursion |
| Binary payload convention | spec §3.5 — UTF-8 string, or base64 with `encoding = "base64"` |
| Lifecycle events | spec §3.6 — the core emits `noisy.*` events for boot, services, bus lagging, scripts, shutdown |

---

## 3. Delivery order

Twelve phases (0 through 11), each with acceptance criteria. Phases 1–3 are the core and are
never parallel with module work. The C++ SDK has its own phase (5) so it
cannot destabilize core milestones; automation arrives last (9) as the
largest independent module.

| Phase | Name | Delivers |
|---|---|---|
| 0 | Repository bootstrap | workspace, CI, dependency governance |
| 1 | Core without Lua | bus, descriptor, module trait, registry, actor runtime |
| 2 | Lua runtime | dispatch bridge, descriptor-generated namespaces, stdlib loader |
| 3 | Engine + plugin loader | the `noisy` binary, C ABI loader, app entry, signals |
| 4 | Foundation modules | `log`, `timer`, `fs`, `crypto` |
| 5 | C++ SDK | `sdk/cpp/noisy.hpp`, examples, CI toolchain |
| 6 | `config` + application layer | config module, `noisy.use` gating, Lua stdlib, first runnable app |
| 7 | I/O modules | `process`, `net` |
| 8 | `serial` + spec examples | serial module, `serial_to_net`, `data_pipeline` |
| 9 | Hot reload + dev loop | transactional script reload, `dev.sh`, `test_lua.py` |
| 10 | `automation` | CDP module, `web_monitor` |
| 11 | Hardening + release | benchmarks, docs set, v0.1.0 |

---

## 4. Phases

### Phase 0 — Repository bootstrap

- Cargo workspace with `noisy_core`, `noisy_ffi`, `engine` skeletons;
  workspace dependency pins.
- Clippy warnings-as-errors, `cargo fmt`, cargo-deny license allowlist
  (MIT / Apache-2.0 / BSD / MPL-2 / ISC per spec §7).
- CI: build, test, clippy, deny, docs.

**Acceptance:** empty workspace green in CI; dependency governance enforced.

### Phase 1 — Core without Lua (bus, descriptor, trait, registry)

- `bus.rs`: `NoisyEvent` envelope (epoch-ms timestamp), broadcast channel with
  configurable capacity, lagged-receiver policy, core emit helper, `noisy.*`
  lifecycle event constants.
- `module.rs`: `ModuleDescriptor` (validation: unique names, method/signal
  naming, topological `depends`), `NoisyModule` trait, `ModuleContext`
  (commands, host services, child token, config), `HostCtx`
  (`emit / call / call_async / on / invoke_callback / log / now_ms`).
- Actor runtime: one Tokio task per static module; the thread-based runner
  for dynamic modules arrives with the ABI in Phase 3.
- `registry.rs`: `load`, lifecycle events, ordered shutdown with per-module
  timeout, duplicate-name rejection.
- Mock modules: event exchange, command round-trips, one slow-drainer to
  exercise the timeout path.

**Acceptance:**
- Modules exchange events and commands; clean shutdown under root-token
  cancel; a slow module is force-joined after the timeout without hanging.
- Cross-module `call` works; a re-entrant recursive `call` is rejected with a
  precise error; the `call_async` path completes.
- Callback tokens round-trip through `invoke_callback` against a test stub.

### Phase 2 — Lua runtime and dispatch bridge

- mlua 5.4, safe standard libraries (all except `io`; restricted `os`);
  serde JSON boundary layer (the single converter).
- Dispatch loop + job inbox (`BusEvent | InvokeCallback | RunSource |
  ReloadScript | Eval | Shutdown`); all Lua calls inside `pcall`; errors
  become `noisy.script_error` { path, line, message }. Timer callbacks
  arrive as callback tokens, sharing the `InvokeCallback` path.
- **Namespace generator:** descriptor → Lua global table. Positional Lua
  arguments pack into declared method params; a Lua function argument becomes
  a callback token; replies become tables. Global `noisy`:
  `on / off / emit / use / version / reload / shutdown`.
- Embedded stdlib loader (`lua/` via `include_str!` + custom `require`
  searcher).

**Acceptance:**
- Harness app subscribes to 100 mock events, unsubscribes cleanly, emits
  Lua-side events; a failing handler emits `noisy.script_error` and the loop continues.
- Generator test: a descriptor with `(name, options, callback)`-shaped
  methods round-trips arg packing and callback invocation.
- Debug assertion (enabled in CI) proves no Lua call happens off the Lua thread.

### Phase 3 — Engine binary and plugin loader (C ABI v1)

- CLI `noisy <app-dir>` with `--entry`, `--log-level`, `--capacity`,
  `--shutdown-timeout`, `--no-hot-reload`, `--check`; engine TOML config
  parsed before any module load.
- Boot: engine config → bus + registry → in-tree modules → **`.so` plugins**
  (search path `$NOISY_MODULE_PATH` / `./modules/` / config list; ABI version
  and descriptor validated at load; names unique) → per-dynamic-module actor
  thread runner (command queue + poll fd watch + tick) → Lua runtime →
  generated namespaces → app entry (`app.lua` or module entry per spec §6.5)
  → `noisy.ready`.
- SIGINT/SIGTERM → registry shutdown → exit 0.
- A Rust `cdylib` test module proves the plugin path before any C++ exists.
- `--check`: engine config + Lua syntax validation without spawning.

**Acceptance:**
- Trivial app starts, runs, and exits cleanly under SIGINT mid-flight.
- The cdylib test module loads; its methods are callable from Lua; events
  reach it; shutdown is clean.
- A wrong-ABI or wrong-descriptor plugin fails with a readable message;
  `--check` catches bad app scripts without running modules.

### Phase 4 — Foundation modules: `log`, `timer`, `fs`, `crypto`

Built through the full authoring path (descriptor + command handling) as the
model's validation. `log` mirrors `log.entry` onto the bus; `timer` keeps an
id registry for `after / every / cancel / now / sleep` and invokes Lua
handlers via callback tokens; `fs` implements the full spec §4.3 surface with
inotify watches; `crypto` is synchronous and event-free.

**Acceptance:** per-module command round-trips and event assertions; fs
watch ordering test; `timer.sleep` parks the calling Lua job and the
turnaround benchmark is recorded in §5; each foundation module stays small
(the guide's baseline size).

### Phase 5 — C++ SDK, examples, CI toolchain

- `sdk/cpp/noisy.hpp`: header-only `NOISY_MODULE` macro, `noisy::Event`,
  `bus.on(...)`, `ctx.emit / call / call_async / log / now_ms` wrappers.
- `examples/cpp_module/` (C++ module + Lua app) and `examples/cpp_app/`
  (C++ application as module entry); per-module CMake scaffold and one-line
  manual build documented in the module guide.
- `gen_module.py` gains the C++ template; CI gains a pinned C++ stage
  building all examples with warnings-as-errors.

**Acceptance:**
- cpp_module e2e: Lua calls a C++ method and gets a structured reply; a C++
  slot receives a bus event and answers back into Lua.
- cpp_app e2e: a C++ module as the application runs to completion, exit 0.
- No SDK usage requires any bridge tool or root build system.

### Phase 6 — `config` module, gating, Lua stdlib, first runnable app

- `config`: TOML/JSON load, dot-notation `get` with default, `set`, `all`,
  `watch` via fs events, `key_changed` diffing, error events.
- `noisy.use` gating: autoload metatable errors on unloaded module globals
  with an advisory message; unknown names validated eagerly.
- Lua stdlib: `util.lua` (split/trim/starts_with/ends_with/deep_copy/
  timestamp/retry), `json.lua`, `event.lua` (`once`, `wait_for`, `pipe`),
  `test.lua` (describe/it/expect, exit-code runner).
- `examples/hello_service/`: heartbeats, config reads, file writes, config
  change reaction.

**Acceptance:** hello_service e2e asserts heartbeat count, a live
`config.key_changed` delivery, and written file content; `noisy.use("serial")`
against an engine built without serial yields the advisory error, not a
nil-table crash.

### Phase 7 — I/O modules: `process`, `net`

- `process` per spec §4.5 (PTY-free in v1); `run_stream` line callbacks use
  `invoke_callback`.
- `net` per spec §4.2: TCP client/server, UDP bind/send, TLS via rustls
  feature, HTTP client via hyper; listeners expose per-connection child
  tokens for clean accept-loop shutdown.

**Acceptance:** process e2e (echo round-trip, signals, streaming callback
order, cwd/env); net e2e (TCP echo on ephemeral ports, TLS with a self-signed
fixture, HTTP against a local test server, disconnect reasons, task and actor
thread counts stable across shutdown).

### Phase 8 — `serial` and the spec examples

- `serial` per spec §4.1: `serialport` backend, per-port fd wait in the actor
  (no busy loop), read timeouts, byte payloads per the spec §3.5 convention,
  port-pair abstraction for CI injection, pty-pair e2e without hardware.

**Acceptance:**
- `serial_to_net` e2e: bytes into pty A surface as JSON on TCP side B.
- `data_pipeline` e2e: fs events in a demo directory become HTTP posts to a
  local test server.
- Zero hardware dependencies in CI; graceful skip with explanation when no
  ptys are available.

### Phase 9 — Hot reload and the dev loop

- Script inventory with stable ids; subscriptions owner-tagged per script.
- Reload: fs events on script directories → inbox `ScriptReload(path)` → drop
  owner-tagged subscriptions → re-evaluate in `pcall` → on failure restore
  the previous state → `noisy.script_reloaded { path }`. Only application and
  plugin scripts reload; module namespaces and compiled modules are
  persistent.
- `scripts/bash/dev.sh` (build + run with reload);
  `scripts/python/test_lua.py` (Lua suites through the engine; nonzero exit
  on failure).

**Acceptance:** e2e covers spec §5.4 fully — handler edits swap live, hardware
connections survive, a broken script restores the previous state, and the
process keeps running.

### Phase 10 — `automation` (CDP) and `web_monitor`

- Chromium child-process management (temp profile, cleanup on shutdown), CDP
  over WebSocket with typed request/correlation mapping, sessions,
  screenshots to files, spec §4.7 API and signals.
- `examples/web_monitor/` e2e with headless chromium (cached toolchain in CI).

**Acceptance:** web_monitor e2e navigates a bundled static site and asserts
`get_text`, `click`, `type_text`, session isolation, and screenshot output;
missing-Chromium environments yield clear `automation.error` events and skip
e2e with an explanation.

### Phase 11 — Hardening and the 0.1.0 release

- Benchmarks: bus fan-out (1000 events/s, 8 subscribers), Lua dispatch hop
  latency, C++ module round-trip (emit → Lua slot → emit); capacity defaults
  frozen from results into this plan's §5.
- Robustness audit: panic-free shutdown in every module, thread/task leak
  audit, unwind profile pinned, final cargo-deny report.
- Docs set completed and verified against implementations:
  `ARCHITECTURE.md`, `LUA_API.md`, `MODULE_GUIDE.md`, `EXTENSION_GUIDE.md`,
  `LUA_API.md`, `MODULE_GUIDE.md`, `EXTENSION_GUIDE.md`,
  `ABI.md`, `EXAMPLES.md`.
- Version 0.1.0 tagged; the specification's stability tiers (§9) take effect.

---

## 5. Open measurement points (resolved during delivery)

| Point | Where decided |
|---|---|
| `timer.sleep`: parked-job vs simple-block implementation | Phase 4 (recording below) |
| Bus capacity and command channel defaults | Phase 11 benchmarks |
| Shutdown settle time default (spec names a configurable value) | Phase 11 |

**`timer.sleep` — parked-job (coroutine suspension), chosen in Phase 4.**
Every Lua entry point (entry chunks, event handlers, callback invocations)
runs inside a fresh Lua coroutine. `timer.sleep(ms)` allocates a wake token,
registers the calling coroutine under it, schedules the resume on the tokio
runtime, and yields; the dispatch loop keeps serving events and timers
throughout. Measured (asserted as an acceptance test in `noisy_timer`):
event turnaround during a 150 ms sleep is < 50 ms — a simple-block
implementation would measure ≥ 150 ms, with the dispatch loop frozen for
the whole sleeper wait. The registry ids of parked sleeps are the Lua
runtime's wake tokens; the `timer` module keeps refusing native
(non-Lua) `sleep` calls with a readable pointer to `timer.after`. Known
limitation: a yield cannot cross a C-call boundary, so `timer.sleep` at
`require` time fails readably instead of corrupting state.

---

## 6. Test and CI strategy

| Layer | Where | Runner |
|---|---|---|
| Core unit + lifecycle | `tests/rust/` | `cargo test` |
| Module tests | module crates | `cargo test` |
| C ABI / loader | `tests/rust/abi_*` + Rust cdylib module | `cargo test` |
| C++ SDK and modules | `tests/cpp/` | CI C++ stage driving the real engine |
| Lua suites | `tests/lua/*.lua` with `test.lua` | `scripts/python/test_lua.py` |
| Application e2e | `tests/e2e/` driving the built binary | CI shell |

CI: GitHub Actions, Linux only (spec §10), toolchain-pinned matrix, hardware
and external-network free.

---

## 7. Risk register

| Risk | Mitigation |
|---|---|
| Lua-thread deadlock (a handler calls a module that emits back) | No re-entrancy across the boundary; emits queue to the inbox; stress tests in Phases 2 and 5 |
| Re-entrant cross-module calls deadlock | Runtime reentrance rejection + `call_async` for recursion (spec §6.1); awareness tests in Phase 1 |
| A malformed `.so` crashes the process | ABI version gate at load, descriptor validation, host-copied replies (no shared allocator); out-of-process runner is the planned containment upgrade |
| Descriptor drift against generated bindings and docs | Descriptors are the single source; CI verifies generated namespaces and `docs/LUA_API.md` extracts |
| Binary data breaks the JSON boundary | Spec §3.5 convention enforced at the boundary; violations become error events |
| Hot reload leaves inconsistent state | Transactional reload with snapshot restore; Phase 9 acceptance test |
| Callback tokens leak | Sweeps on script unload and shutdown; weak-reference registration |
| Chromium/build variance in CI | Automation is last on the path; e2e skips with explanation when absent |
| ABI freezes too early | The ABI is validated by mock + cdylib modules before the Phase 3 gate; versioned from the first commit via `docs/ABI.md` |
| Serial/automation depend on environment specifics | Port-pair abstraction + pty pairs; headless chromium with cache |

---

## 8. Beyond 0.1.0 (planned extensions, per spec)

- Out-of-process module runner (crash containment, supervision restarts) —
  same ABI messages over UDS/JSON.
- Additional module languages (Python, Go, ...) via the runner protocol.
- Full async Lua coroutines; Lua-file configuration; per-method typed
  adapter generation via scaffolding.
- PTY-backed process support; non-Linux platforms.

---

## 9. Immediate next steps

1. Phase 0: workspace scaffold (`noisy_core`, `noisy_ffi`, `engine`), CI,
   cargo-deny, clippy configuration.
2. `docs/ABI.md`: draft the C ABI policy page alongside Phase 1; the ABI
   freezes only after Phase 2 acceptance passes.
3. Phase 1: `ModuleDescriptor`, `HostCtx`, and the mock-module lifecycle
   tests — the first proof of the descriptor model.
