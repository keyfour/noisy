# noisy Framework Specification

**Version:** 0.3.0-draft
**Status:** Design specification — pre-implementation

---

## 1. What is noisy?

noisy is a single-process, binary-native application framework for Linux,
built on community-owned technology with no single corporate dependency.

It provides the same value proposition as established proprietary frameworks:
a cohesive programming model with signals, slots, typed modules, and a
high-level extension language — so developers design *what* their application
does, not *how* memory, threads, and I/O are managed.

The core philosophy: the framework absorbs systems complexity. The developer
works at the design level.

---

## 2. Core Concepts

### 2.1 The Three Layers

```
┌──────────────────────────────────────────────────────────────┐
│                      Lua Extension Layer                      │
│                                                              │
│  Application logic, configuration, module composition,       │
│  event reactions, task orchestration. Hot-reloadable.        │
│  No build step. Accessible to non-systems developers.        │
│                                                              │
│  app.lua   config.lua   tasks/*.lua   plugins/*.lua          │
└───────────────────────────────┬──────────────────────────────┘
                                │  stable, versioned API
┌───────────────────────────────▼──────────────────────────────┐
│                    noisy Module Layer                         │
│                                                              │
│  Self-contained capability modules. Each module owns its     │
│  state, emits signals, serves methods, and declares its      │
│  public type in a descriptor. Written in Rust or C++.        │
│                                                              │
│  net  serial  fs  timer  automation  process  crypto  ...   │
└───────────────────────────────┬──────────────────────────────┘
                                │
┌───────────────────────────────▼──────────────────────────────┐
│                      noisy Core                               │
│                                                              │
│  Event bus, module registry, descriptor bindings, service    │
│  lifecycle, Lua runtime, scheduler. Frozen infrastructure.   │
│                                                              │
│  Tokio event loop · CancellationToken tree · mlua            │
└──────────────────────────────────────────────────────────────┘
```

### 2.2 noisy as a Qt alternative

noisy plays the same architectural role as Qt for software that does not need
a GUI:

| Qt model              | noisy model                              |
|-----------------------|------------------------------------------|
| C++ (first-class)     | Rust **and** C++ (both first-class)      |
| QML                   | Lua                                       |
| signal emission       | `emit(kind, payload)` — bus broadcast    |
| slots               | connected handlers, any language, runtime |
| method call on object | `call(module, method, params) → reply`   |
| Q_OBJECT              | module descriptor (runtime metadata)     |
| moc compiler          | descriptor + host binding — no codegen   |
| Qt modules            | noisy modules (net, serial, fs, ...)     |
| single process        | single process                            |

Lua in noisy is **not a UI language**. It is a general logic and composition
layer. noisy targets applications, services, automation, embedded systems, and
tooling.

### 2.3 Signals, Slots, and Methods

**Signals** — typed, broadcast, fire-and-forget.
A module emits a signal onto the bus. Any number of listeners may react. The
emitter does not know who is listening.

**Slots** — handlers connected to signals at runtime, from any language.
Each `on()` returns a connection id; `off(id)` disconnects.

**Methods** — typed, directed, request/reply.
A caller invokes a method on a specific module and awaits a reply. The caller
knows which module it is talking to.

```lua
local data = serial.read(port, 64)   -- method call, directed to the serial module
local page  = browser.navigate(url)  -- method call, directed to the automation module

noisy.on("serial.data_received", function(event)
    log.info("received: " .. event.payload.data)
end)

noisy.on("net.disconnected", function(event)
    timer.after(5000, function() net.connect(event.payload.host, 8080) end)
end)
```

This maps directly to Qt's signal/slot model without code generation, without
a compiler step, and with the wiring expressed in the developer's language.

### 2.4 The Module System

A module is a self-contained capability unit that:

- Has a unique name (`serial`, `net`, `automation`, `timer`, `fs`, `log`)
- Declares its public type in a **descriptor**: name, version, signals,
  methods with parameter names, and dependencies
- Owns its internal state exclusively
- Emits signals and serves methods through the bus
- Has a defined lifecycle: `init → start → running → stop → cleanup`
- Can be written in Rust or C++ — indistinguishable to the caller

The Lua layer sees every loaded module's namespace generated from its
descriptor:

```lua
serial.open("/dev/ttyUSB0", { baud = 115200 })
net.connect("192.168.1.1", 8080)
fs.watch("/var/data")
```

Applications declare which modules they use with `noisy.use(...)`; an
autoloaded metatable reports a clear advisory if an unloaded module is touched.

---

## 3. Architecture Specification

### 3.1 noisy Core

The core is the part that never changes in production. It provides:

**Event Bus**
A `broadcast` channel carrying `NoisyEvent` values. All modules publish to it.
All subscribers listen on it. Capacity is configurable. A lagged subscriber
receives a `noisy.bus_lagged` warning and resumes from the latest event.

**Module Registry**
Owns all running module handles. Provides:
- `load(factory_or_abi)` — start and track a module (in-tree or dynamic)
- `get(name)` — retrieve a handle by name
- `shutdown(timeout_ms)` — cancel all tokens, drain, join with a timeout,
  and report any module that failed to stop

**Lua Runtime**
A single `mlua::Lua` instance owned by a dedicated thread. The core:
- Opens safe standard libraries (all except `io`; `os` restricted)
- Generates each module's global namespace **from its descriptor**
- Provides `noisy.on/off/emit/use/version/shutdown`
- Manages the dispatch loop that invokes Lua callbacks inside `pcall`
- Supports `require()` for the stdlib (`lua/`) and application plugins

**Scheduler**
Every module runs as an independent task: a Tokio task for in-tree Rust
modules, or one host-owned actor thread for dynamic `.so` modules. The Lua
runtime runs in a dedicated blocking thread bridged to the Tokio world via
`block_on`. Commands emitted by one module never block another.

**Lua callback tokens**
A Lua function passed into any method call is wrapped as an opaque token
(`cb:<id>`); modules invoke it through the host context (`invoke_callback`),
which queues a thread-safe job on the Lua dispatch thread. This makes
`timer.after(ms, fn)`-style APIs uniform in every language.

**CancellationToken Tree**
One root token; each module gets a child. Cancelling the root propagates
simultaneously. No module can block shutdown indefinitely.

### 3.2 Module Lifecycle

```
          ┌──────────┐
          │   init   │  ← constructor, allocate resources, validate config
          └────┬─────┘
               │
          ┌────▼─────┐
          │  start   │  ← begin serving, emit a module-started signal
          └────┬─────┘
               │
          ┌────▼──────────────────────────────────┐
          │             running                    │
          │                                        │
          │   select! {                            │
          │     cmd  = commands.recv() → dispatch()│
          │     _    = shutdown()       → break    │
          │   }                                    │
          └────┬──────────────────────────────────┘
               │  cancellation
          ┌────▼─────┐
          │  drain   │  ← finish in-flight commands
          └────┬─────┘
               │
          ┌────▼─────┐
          │ cleanup  │  ← release resources, close connections
          └────┬─────┘
               │
          ┌────▼─────┐
          │ stopped  │  ← emit a module-stopped signal, task/join resolves
          └──────────┘
```

### 3.3 The Module Descriptor

The descriptor is a module's public type — the replacement for compiled
metadata. The engine validates it at load (unique names, topological
dependency check) and derives all bindings from it.

```jsonc
{
  "name": "serial",
  "version": "0.1.0",
  "signals":  ["opened", "closed", "data_received", "error", "timeout"],
  "methods": [
    { "name": "open", "params": ["port", "options"] },
    { "name": "read", "params": ["port", "n", "timeout_ms"] }
  ],
  "depends": []          // other module names, ordered at boot
}
```

The generated Lua namespace is mechanical:

```
{ "name": "open", "params": ["port", "options"] }
  →  serial.open("/dev/ttyUSB0", { baud = 115200 })

{ "name": "read", "params": ["port", "n", "timeout_ms"] }
  →  serial.read("/dev/ttyUSB0", 64, 1000)
```

Positional Lua arguments are packed into the declared parameters; a Lua
function argument becomes a callback token; JSON replies convert to tables.

### 3.4 The Module Interface (two equivalent authoring paths)

Every module implements the same conceptual model through one of two paths
that share one wire format:

**In-tree Rust — static, typed internally:**

```rust
pub trait NoisyModule: Send + 'static {
    /// Public type of the module
    fn descriptor(&self) -> ModuleDescriptor;

    /// Serve commands until shutdown. Delivered one at a time on a Tokio task.
    fn start(self: Box<Self>, ctx: ModuleContext) -> JoinHandle<()>;
}

pub struct ModuleContext {
    pub commands: CommandReceiver,      // (method, params, reply channel)
    pub host: HostCtx,                  // emit, call, call_async, on, invoke_callback, log, now_ms
    pub shutdown: CancellationToken,    // child token
    pub config: serde_json::Value,      // per-module configuration
}
```

The `Box<Self>` receiver keeps the trait object-safe: modules are stored and
loaded as `Box<dyn NoisyModule>`.

**Third-party C++ (or Rust) — dynamic `.so` over the C ABI** (Section 6):
`noisy_module_create / command / tick / event / reply / stop / destroy`.
The engine assigns one actor thread per dynamic module and delivers exactly
one message at a time. A dynamic module never locks and never spawns threads.

Both paths speak the same JSON command/event vocabulary; typed structs live
inside each language and convert once at the boundary.

### 3.5 Event Schema

All signals share a common envelope:

```rust
pub struct NoisyEvent {
    pub source:    String,              // "serial"
    pub kind:      String,              // "data_received"
    pub name:      String,              // "serial.data_received"
    pub payload:   serde_json::Value,
    pub timestamp: u64,                 // epoch milliseconds
}
```

Lua receives events as tables:

```lua
noisy.on("serial.data_received", function(event)
    print(event.source)             -- "serial"
    print(event.kind)               -- "data_received"
    print(event.payload.port)       -- "/dev/ttyUSB0"
    print(event.payload.data)       -- binary-safe payload (see below)
end)
```

**Binary convention:** a byte-carrying payload field is a UTF-8 string when
the bytes are valid UTF-8. Otherwise the field carries base64 and a sibling
`encoding` field is set to `"base64"`. Violations surface as module error
events, never as silent truncation.

### 3.6 Core Lifecycle Signals

Defined in `noisy_core/src/bus.rs` and emitted by the core itself
(`source = "noisy"`):

```
noisy.started            { version }
noisy.ready              { modules }
noisy.service_started    { module }
noisy.service_stopped    { module }
noisy.shutdown_timeout   { module }
noisy.bus_lagged         { subscriber, missed }
noisy.script_loaded      { path }
noisy.script_reloaded    { path }
noisy.script_error       { path, line, message }
noisy.shutdown
```

---

## 4. Standard Module Specifications

### 4.1 `serial` — Serial Port Communication

**Methods**
```lua
serial.open(port, options)       -- options: { baud, data_bits, stop_bits, parity }
serial.close(port)
serial.write(port, data)         -- bytes (string)
serial.read(port, n, timeout_ms) -- read up to n bytes
serial.read_line(port, timeout_ms)
serial.set_baud(port, rate)
serial.list()                    -- → list of available ports
```

**Signals**
```
serial.opened           { port }
serial.closed           { port }
serial.data_received    { port, data, length }
serial.error            { port, error }
serial.timeout          { port }
```

**Rust internals:** `serialport` crate. Each open port waits efficiently on
its file descriptor (poll) inside the module actor, feeding data into the
event bus. Test builds inject a port-pair abstraction; CI needs no hardware.

---

### 4.2 `net` — TCP/UDP Networking

**Methods**
```lua
net.connect(host, port, options)    -- TCP client; options: { tls, timeout_ms, reconnect }
net.listen(port, options)           -- TCP server; options: { tls, backlog }
net.udp_bind(port)
net.udp_send(udp_id, host, port, data)
net.send(conn_id, data)
net.close(conn_id)
net.http_get(url, headers)          -- → { status, body, headers }
net.http_post(url, body, headers)
net.http_request(options)           -- full control: { method, url, headers, body }
```

**Signals**
```
net.connected           { conn_id, host, port }
net.disconnected        { conn_id, host, port, reason }
net.data_received       { conn_id, data, length }
net.client_accepted     { conn_id, remote_addr }
net.error               { conn_id, error }
```

**Rust internals:** Tokio TCP/UDP, `rustls` for TLS (feature-gated), `hyper`
for the HTTP client.

---

### 4.3 `fs` — Filesystem and File Watching

**Methods**
```lua
fs.read(path)                 -- → string
fs.read_bytes(path)           -- → string (raw bytes)
fs.write(path, content)
fs.append(path, content)
fs.delete(path)
fs.exists(path)               -- → bool
fs.list(path)                 -- → list of strings
fs.mkdir(path)
fs.move(src, dst)
fs.copy(src, dst)
fs.stat(path)                 -- → { size, modified, is_dir, is_file }
fs.watch(path)
fs.unwatch(path)
fs.glob(pattern)              -- → list of matching paths
```

**Signals**
```
fs.file_created         { path }
fs.file_modified        { path }
fs.file_deleted         { path }
fs.dir_created          { path }
fs.dir_deleted          { path }
fs.watch_error          { path, error }
```

**Rust internals:** `tokio::fs`, `inotify` crate.

---

### 4.4 `timer` — Timing and Scheduling

**Methods**
```lua
timer.after(ms, fn)           -- call fn once after ms milliseconds → timer_id
timer.every(ms, fn)           -- call fn repeatedly → timer_id
timer.cancel(timer_id)
timer.now()                   -- → epoch milliseconds
timer.sleep(ms)               -- parks the calling Lua job; the dispatch
                              -- loop keeps serving events and other timers
```

**Signals**
```
timer.fired             { timer_id, elapsed_ms }
timer.cancelled         { timer_id }
```

**Rust internals:** `tokio::time::interval` and `tokio::time::sleep`; Lua
functions invoked through callback tokens.

---

### 4.5 `process` — Process and Shell Management

**Methods**
```lua
process.run(cmd)              -- run shell command, collect output
                              -- → { stdout, stderr, exit_code }
process.run_stream(cmd, fn)   -- run, call fn(line, stream) per output line
process.spawn(cmd)            -- spawn detached process → pid
process.kill(pid, signal)
process.env(name)
process.env_all()
process.cwd()
process.chdir(path)
```

**Signals**
```
process.started         { pid, cmd }
process.finished        { pid, cmd, exit_code }
process.output          { pid, line, stream }   -- "stdout" | "stderr"
process.killed          { pid, signal }
```

**Rust internals:** `tokio::process`.

---

### 4.6 `log` — Structured Logging

**Methods**
```lua
log.debug(msg, fields)
log.info(msg, fields)
log.warn(msg, fields)
log.error(msg, fields)
log.set_level(level)          -- "debug" | "info" | "warn" | "error"
```

**Signals**
```
log.entry               { level, msg, fields, timestamp }
```

**Rust internals:** `tracing` crate with a configurable subscriber. Log
entries also flow through the event bus so scripts can react to them.

---

### 4.7 `automation` — Web and UI Automation

Browser interaction over the Chrome DevTools Protocol. One module among many.

**Methods**
```lua
automation.navigate(url)
automation.get_text(selector)
automation.get_attr(selector, attr)
automation.click(selector)
automation.type_text(selector, text)
automation.submit(selector)
automation.evaluate(js)               -- → string (JSON)
automation.content()
automation.screenshot(path)
automation.wait_for(selector, ms)
automation.new_session()
automation.close_session(session_id)
```

**Signals**
```
automation.page_loaded          { url }
automation.page_changed         { url }
automation.navigation_failed    { url, error }
automation.element_found        { selector }
automation.element_not_found    { selector }
automation.download_complete    { path }
```

**Rust internals:** CDP over WebSocket (`tokio-tungstenite`), Chromium as a
managed child process.

---

### 4.8 `crypto` — Cryptographic Primitives

**Methods**
```lua
crypto.hash(algorithm, data)      -- sha256, sha512, md5 → hex string
crypto.hmac(algorithm, key, data)
crypto.base64_encode(data)
crypto.base64_decode(data)
crypto.random_bytes(n)
crypto.uuid()
```

**No signals emitted.** Pure synchronous transformation module.
**Rust internals:** `ring` or `sha2`/`hmac` crates.

---

### 4.9 `config` — Configuration Management

**Methods**
```lua
config.load(path)             -- TOML or JSON, detected by extension
config.get(key, default)      -- dot notation: "server.port"
config.set(key, value)        -- runtime override
config.all()
config.watch(path)            -- reload on change (fs.watch integration)
```

**Signals**
```
config.loaded           { path }
config.reloaded         { path }
config.key_changed      { key, old_value, new_value }
config.error            { path, error }
```

**Rust internals:** `toml` and `serde_json` crates.

---

## 5. Lua Extension Layer Specification

### 5.1 noisy Standard Library

The `lua/` directory ships with noisy and is always available via `require`:

```
lua/
├── util.lua        -- string helpers, retry, timestamps
├── json.lua        -- JSON encode/decode (wraps the core boundary)
├── log.lua         -- logging shortcuts
├── event.lua       -- event subscription helpers
└── test.lua        -- lightweight test runner for task scripts
```

### 5.2 Application Entry Point

Every noisy application has an entry point script. By convention `app.lua`:

```lua
-- app.lua — application entry point

config.load("config.toml")

local conn = net.connect(config.get("server.host"), config.get("server.port"))

serial.open(config.get("device.port"), {
    baud = config.get("device.baud")
})

noisy.on("serial.data_received", function(event)
    local parsed = parse_frame(event.payload.data)
    net.send(conn, json.encode(parsed))
    log.info("forwarded frame", { bytes = event.payload.length })
end)

noisy.on("net.disconnected", function(event)
    log.warn("server disconnected, reconnecting in 5s")
    timer.after(5000, function()
        conn = net.connect(config.get("server.host"), config.get("server.port"))
    end)
end)

timer.every(60000, function()
    log.info("heartbeat", { uptime = timer.now() })
end)
```

This is the entire application — no Rust or C++ code, no build step. The
developer is designing what the application does; the framework handles the
how.

### 5.3 Lua Module Extensions

Third parties can ship Lua-only modules that compose built-in modules:

```lua
-- plugins/mqtt.lua
local M = {}
local conn_id = nil

function M.connect(host, port, options)
    conn_id = net.connect(host, port or 1883)
    net.send(conn_id, mqtt_connect_packet(options))
end

function M.subscribe(topic, fn)
    noisy.on("net.data_received", function(event)
        local msg = mqtt_parse(event.payload.data)
        if msg and msg.topic == topic then fn(msg) end
    end)
    net.send(conn_id, mqtt_subscribe_packet(topic))
end

function M.publish(topic, payload)
    net.send(conn_id, mqtt_publish_packet(topic, payload))
end

return M
```

```lua
local mqtt = require("plugins/mqtt")
mqtt.connect("broker.example.com")
mqtt.subscribe("sensors/#", function(msg)
    log.info("sensor reading", { topic = msg.topic, value = msg.payload })
end)
```

No Rust, no C++, no compile step. The plugin ships as a `.lua` file.

### 5.4 Hot Reload

noisy watches application script directories via the `fs` module. When a
script changes:

1. The changed script is re-evaluated in the existing Lua state
2. Event subscriptions owned by the old version are dropped and re-created
3. Module namespaces, connections, and hardware state remain valid — no
   reconnection needed
4. A `noisy.script_reloaded` event fires

Only application and plugin scripts reload. A script that fails to load is
restored to its previous state; the engine reports the error and continues.

### 5.5 Error Handling in Lua

Script errors are isolated and never crash the engine:

```lua
noisy.on("serial.data_received", function(event)
    local ok, err = pcall(function()
        process_frame(event.payload.data)
    end)
    if not ok then
        log.error("frame processing failed", { error = err })
    end
end)
```

Unhandled errors in scripts emit a `noisy.script_error` event with the file,
line number, and message. The engine continues running.

---

## 6. Module Authoring in Rust and C++

Rust and C++ are both first-class languages for modules and applications. The
engine consumes both through the same conceptual model; the descriptor and
JSON wire format are shared.

### 6.1 Host Services

Every module receives a host context with the same services in both
languages:

| Service | Purpose |
|---|---|
| `emit(kind, payload)` | publish a signal to the bus (thread-safe) |
| `call(module, method, params, timeout)` | synchronous request/reply to another module |
| `call_async(module, method, params)` | fire a command; the reply arrives on the module's command stream |
| `on(pattern)` | connect a slot; the returned stream disconnects when dropped |
| `invoke_callback(token, payload)` | call a language-native handler (e.g. a Lua closure) |
| `log(level, msg, fields)` | emit a structured log entry |
| `now_ms()` | epoch milliseconds |
| `module_name()` | this module's own name |

The shutdown indicator is not a host service: every module receives it
directly as `ctx.shutdown`, a child token of the engine's root token.

Cross-module calls made inside another module's command dispatch that would
re-enter that same module are rejected at runtime; use `call_async` for
mutual recursion.

### 6.2 The C ABI — the single dynamic-module contract

```c
#define NOISY_ABI_VERSION 1

typedef struct noisy_ctx noisy_ctx;

typedef struct noisy_host {          /* provided by the engine; thread-safe unless noted */
    int  (*emit)(noisy_ctx*, const char* kind, const char* payload_json);
    int  (*invoke_callback)(noisy_ctx*, uint64_t token, const char* payload_json);
    int  (*subscribe)(noisy_ctx*, const char* pattern);
    int  (*call_async)(noisy_ctx*, const char* module, const char* method,
                       const char* params_json);
    int  (*reply)(noisy_ctx*, int status, const char* reply_json); /* host copies; once per command */
    int  (*log)(noisy_ctx*, int level, const char* msg, const char* fields_json);
    uint64_t (*now_ms)(noisy_ctx*);
    int*     shutdown_flag;        /* atomic, read-only for the module */
} noisy_host;

/* module exports */
const char* noisy_abi_version(void);
const char* noisy_module_descriptor(void);    /* descriptor JSON */
noisy_ctx*  noisy_module_create(noisy_host host);
int  noisy_module_command(noisy_ctx*, const char* method, const char* params_json);
int  noisy_module_tick(noisy_ctx*, uint64_t now_ms);          /* optional heartbeat */
int  noisy_module_event(noisy_ctx*, const char* name, const char* payload_json);
int  noisy_module_reply(noisy_ctx*, uint64_t call_token, const char* reply_json);
int  noisy_module_stop(noisy_ctx*, uint64_t timeout_ms);
void noisy_module_destroy(noisy_ctx*);
```

Contract rules:

- The engine owns one **actor thread** per dynamic module and delivers
  `command / event / tick / reply` one at a time. Modules never lock and
  never spawn threads.
- `emit / invoke_callback / call_async / reply / log / subscribe` may be
  called from anywhere; the host makes them thread-safe.
- `noisy_watch(fd, interest)` (Linux `poll`) lets an actor thread wait
  efficiently on file descriptors — drivers without busy loops.
- `reply()` must be called exactly once per command, on the actor thread.
- Nothing crosses the boundary as shared memory. All data is JSON strings;
  byte payloads follow the Section 3.5 convention. The host copies all
  replies; no allocator is shared.
- `noisy_abi_version()` must match the engine exactly; the versioning policy
  lives in `docs/ABI.md`.

### 6.3 The C++ SDK

`sdk/cpp/noisy.hpp` is header-only and maps the ABI to Qt-like ergonomics:

```cpp
NOISY_MODULE("sensors", ctx) {
    mod.signals = { "published" };
    mod.methods = { { "poll", { "device" } } };
    mod.depends = { "serial" };

    bus.on("serial.data_received", [&](const noisy::Event& e) {
        ctx.emit("published", { { "n", e.payload["length"] } });
    });

    mod.on_command("poll", [](const nlohmann::json& p) {
        return noisy::ok(json{ { "count", read_device(p["device"]) } });
    });
}
```

```bash
g++ -std=c++20 -shared -fPIC -I<sdk-include> sensors.cpp -o noisy_sensors.so
```

No moc, no bridge declarations, no root CMakeLists. A per-module CMake
scaffold is generated by `scripts/python/gen_module.py` for teams that
prefer it.

### 6.4 Rust Modules

In-tree Rust modules implement the `NoisyModule` trait (Section 3.4) and
deserialize commands into typed structures themselves; the boundary is one
`serde_json` conversion performed once. Third-party Rust modules compile as
a `cdylib` exporting the same C ABI, so the loader has exactly one plugin
format.

### 6.5 Applications in Rust or C++

An application may be written entirely in Rust or C++: the engine config
names a module as the entry point. The engine loads it, calls start, and
lets it wire reactions and make calls like any module. A Lua `app.lua` and a
module app can coexist on the same bus.

---

## 7. Dependency Governance

Every dependency must satisfy at least one:

- Governed by an established open foundation
- Broad community maintenance, no single controlling entity
- Permissive license (MIT, Apache-2.0, BSD, MPL-2, ISC)
- Small enough that forking is realistic if abandoned

| Dependency         | Purpose                       | Governance        |
|--------------------|-------------------------------|-------------------|
| Tokio              | Async runtime                 | Rust Foundation   |
| mlua               | Lua 5.4 embedding             | Community/MIT     |
| tokio-util         | CancellationToken             | Rust Foundation   |
| inotify            | File watching                 | Community/MIT     |
| serde / serde_json | Serialization                 | Community/MIT     |
| tracing            | Structured logging            | Rust Foundation   |
| serialport         | Serial communication          | Community/MPL-2   |
| tokio-tungstenite  | WebSocket (CDP)               | Community/MIT     |
| rustls             | TLS                           | Community/Apache-2.0 |
| hyper              | HTTP client                   | Community/MIT     |
| ring / sha2        | Cryptography                  | Community/ISC/MIT |
| libloading         | Dynamic module loading        | Community/MIT     |
| toml               | TOML parsing                  | Community/MIT     |
| nlohmann/json      | C++ SDK JSON                  | Community/MIT     |

---

## 8. Project Structure

```
noisy/
│
├── Cargo.toml                        ← workspace root
│
├── crates/
│   ├── noisy_core/                   ← event bus, module registry,
│   │   └── src/                        descriptor model, callbacks, scheduler
│   │       ├── lib.rs
│   │       ├── bus.rs
│   │       ├── module.rs
│   │       ├── registry.rs
│   │       ├── callback.rs
│   │       ├── error.rs
│   │       ├── lua_runtime.rs        ← Phase 2
│   │       └── scheduler.rs
│   │
│   ├── noisy_ffi/                    ← C ABI definitions and loader
│   │   ├── include/noisy/module_c.h
│   │   └── src/
│   │
│   ├── engine/                       ← binary wiring: config, boot,
│   │   └── src/                        plugin loading, signals
│   │       ├── main.rs
│   │       ├── app.rs
│   │       └── config.rs
│   │
│   └── modules/
│       ├── serial/
│       ├── net/
│       ├── fs/
│       ├── timer/
│       ├── process/
│       ├── log/
│       ├── automation/
│       ├── crypto/
│       └── config/
│
├── sdk/
│   └── cpp/noisy.hpp                 ← header-only C++ SDK
│
├── lua/                              ← noisy standard Lua library
│   ├── util.lua
│   ├── json.lua
│   ├── log.lua
│   ├── event.lua
│   └── test.lua
│
├── examples/
│   ├── serial_to_net/                ← serial frames to TCP (app.lua + config.toml)
│   ├── web_monitor/                  ← automation-driven monitoring
│   ├── data_pipeline/                ← fs + timer + net pipelines
│   ├── hello_service/                ← smallest app: config/log/timer/fs
│   ├── cpp_module/                   ← C++ module + Lua application
│   └── cpp_app/                      ← C++ application (module entry)
│
├── scripts/
│   ├── bash/
│   │   ├── build.sh
│   │   ├── dev.sh
│   │   └── test.sh
│   └── python/
│       ├── gen_module.py             ← scaffold Rust or C++ module
│       └── test_lua.py
│
├── docs/
│   ├── SPECIFICATION.md              ← this document
│   ├── implementation_plan.md        ← delivery plan
│   ├── LUA_API.md                    ← full Lua API reference
│   ├── MODULE_GUIDE.md               ← writing modules in Rust or C++
│   ├── EXTENSION_GUIDE.md            ← Lua-only extensions
│   ├── ABI.md                        ← C ABI versioning and compatibility
│   └── EXAMPLES.md                   ← annotated example walkthrough
│
└── tests/
    ├── rust/                         ← Rust unit + integration tests
    ├── cpp/                          ← C++ SDK and module tests
    ├── lua/                          ← Lua script tests (test.lua runner)
    └── e2e/                          ← end-to-end application tests
```

---

## 9. Versioning and Stability Guarantees

### Stability tiers

**Core API** (`noisy_core`) — SemVer strict. Breaking changes require a major
version bump and a migration guide.

**Module API** (descriptor surface, callable from Lua and natively) —
SemVer strict per module. Modules version independently.

**C ABI** — version-gated at load. Compatibility policy defined in
`docs/ABI.md`; breaking changes bump `NOISY_ABI_VERSION` and keep the old
version loadable for one minor cycle.

**Module internals** — not public API; may change between minor versions.

**Lua standard library** (`lua/`) — SemVer strict.

### Compatibility promise

A Lua script written for noisy 1.x will run unmodified on any noisy 1.x
release, regardless of whether a module it uses is implemented in Rust or
C++. This is the central guarantee the framework makes to task authors.

---

## 10. Design Decisions

1. **One shared Lua state** owned by a dedicated thread; all scripts run in
   `pcall`; module namespaces are generated tables.
2. **Descriptor-driven modules.** Lua namespaces are generated from
   descriptors; module authors write no binding code in any language.
3. **Static in-tree Rust modules** plus **dynamic C-ABI plugins** (C++ or
   Rust `cdylib`) with the same wire format.
4. **Single process in production.** Out-of-process module isolation over the
   same messages is a planned extension for crash containment and additional
   module languages.
5. **JSON at every language boundary.** Typed structs are internal to each
   language and converted exactly once.
6. **TOML is the primary configuration format** inside the engine; `config`
   accepts TOML or JSON.
7. **Explicit module discovery:** `noisy.use("serial", "net")`; unloaded
   module usage errors with a clear advisory.
8. **Language-native handlers** (Lua closures passed as arguments) travel as
   opaque tokens invoked via `invoke_callback`.
9. **Linux-first.** inotify, poll, and PTY-free process handling in v1.

