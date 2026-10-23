# noisy Module Guide

How to write a noisy module. A module is a self-contained capability unit
with its own state, its signals, its methods, and a declared public type
(the descriptor). Modules can be written in **Rust** (in-tree, linked into
the engine) or **C++** (dynamic `.so` plugin). Lua scripts cannot tell the
difference: every module's Lua namespace is generated from its descriptor.

---

## 1. Pick a Path

| | Rust module | C++ module |
|---|---|---|
| Where it lives | `crates/modules/<name>/` | any source dir, ships as `.so` |
| How it is registered | listed in `engine/src/app.rs` | found in `$NOISY_MODULE_PATH` or `./modules/` |
| Concurrency | a Tokio task served one command at a time | a host-owned actor thread served one message at a time |
| Threading rule | host delivers commands; no shared Lua access | never lock, never spawn threads |
| Build | part of the Cargo workspace | `g++ -shared -fPIC` or the scaffolded CMake |

Both paths produce the same contract: a descriptor, method handlers, and
signals emitted through the host context.

Scaffold either with:

```bash
scripts/python/gen_module.py mysensor --lang rust   # or --lang cpp
```

---

## 2. The Descriptor

The descriptor is the module's public type. Validate-friendly, machine
readable, and the single source from which all bindings and documentation
are generated.

```jsonc
{
  "name": "mysensor",
  "version": "0.1.0",
  "signals": ["reading", "error"],
  "methods": [
    { "name": "read",        "params": ["sensor_id"] },
    { "name": "set_interval", "params": ["ms"] }
  ],
  "depends": ["serial"]
}
```

Conventions:

- Signal names are `[a-z_]+`; the bus delivers them as `mysensor.<kind>`.
- Method parameters are ordered; the generated Lua call packs positional
  arguments into these names.
- `depends` must be a subset of loaded modules; the engine checks it
  topologically at boot.

---

## 3. Host Services

The engine provides every module with the same host context:

| Service | Notes |
|---|---|
| `emit(kind, payload)` | publish a signal; thread-safe, fire-and-forget |
| `call(module, method, params, timeout)` | synchronous request/reply; rejected on re-entrant recursion |
| `call_async(module, method, params)` | command now; reply arrives on this module's delivery thread |
| `on(pattern)` / `off(id)` | connect and disconnect slots |
| `invoke_callback(token, payload)` | invoke a language-native handler (e.g. a Lua closure passed as a method argument) |
| `log(level, msg, fields)` | structured log entry, also mirrored to the bus as `log.entry` |
| `now_ms()` | epoch milliseconds |
| `shutdown_flag()` | read-only indicator that the engine is shutting down |

---

## 4. Rust Module (in-tree)

`crates/modules/mysensor/Cargo.toml`:

```toml
[package]
name    = "noisy_mysensor"
version = "0.1.0"
edition = "2021"

[dependencies]
noisy_core    = { path = "../../noisy_core" }
tokio         = { workspace = true }
tokio-util    = { workspace = true }
serde_json    = { workspace = true }
tracing       = { workspace = true }
```

`src/lib.rs` — a complete module:

```rust
use tokio::task::JoinHandle;
use serde_json::{json, Value};

use noisy_core::{
    NoisyModule, ModuleDescriptor, ModuleContext, ModuleError,
};

pub struct MySensor;

impl NoisyModule for MySensor {
    fn descriptor(&self) -> ModuleDescriptor {
        ModuleDescriptor::new("mysensor")
            .signals(["reading", "error"])
            .method("read", ["sensor_id"])
            .method("set_interval", ["ms"])
            .depends(["serial"])
    }

    fn start(self, ctx: ModuleContext) -> JoinHandle<()> {
        tokio::spawn(async move {
            while let Some(cmd) = ctx.commands.recv().await {
                if ctx.shutdown.is_cancelled() { break; }
                tokio::select! {
                    reply = self.dispatch(cmd.method(), cmd.params(), &ctx) => {
                        cmd.reply(reply);
                    }
                    _ = ctx.shutdown.cancelled() => break,
                }
            }

            // drain in-flight commands, then release resources
            ctx.drain().await;
            tracing::info!("mysensor stopped");
        })
    }
}

impl MySensor {
    async fn dispatch(&self, method: &str, params: Value, ctx: &ModuleContext)
        -> Result<Value, ModuleError>
    {
        match method {
            "read" => {
                let id  = params["sensor_id"].as_str().unwrap_or_default();
                let now = ctx.host.now_ms();
                ctx.emit("reading", json!({ "sensor_id": id, "value": sample() }))?;
                Ok(json!({ "read_at": now }))
            }
            "set_interval" => {
                Ok(json!({ "ms": params["ms"].as_u64().unwrap_or(1000) }))
            }
            _ => Err(ModuleError::unknown_method("mysensor", method)),
        }
    }
}
```

Notes:

- Commands arrive one at a time; the module owns all state and needs no locks.
- A Lua function passed as a parameter arrives as a callback token; call it
  with `ctx.invoke_callback(token, payload)`.
- Replies are `Result<Value, ModuleError>`; the core converts them for Lua.

Register it in `crates/engine/src/app.rs`:

```rust
registry.load(Box::new(MySensor));
```

---

## 5. C++ Module (dynamic `.so`)

`sdk/cpp/noisy.hpp` is header-only. A complete module in one file:

```cpp
// mysensor.cpp
#include <noisy.hpp>

NOISY_MODULE("mysensor", ctx) {
    mod.signals = { "reading", "error" };
    mod.methods = {
        { "read",        { "sensor_id" } },
        { "set_interval", { "ms" } }
    };
    mod.depends = { "serial" };

    mod.on_command("read", [&](const nlohmann::json& p) {
        ctx.emit("reading", { { "sensor_id", p["sensor_id"] },
                              { "value",     sample() } });
        return noisy::ok({ { "read_at", ctx.now_ms() } });
    });

    mod.on_command("set_interval", [&](const nlohmann::json& p) {
        return noisy::ok({ { "ms", p.value("ms", 1000) } });
    });

    // reacting to other modules is an ordinary slot connection
    bus.on("serial.data_received", [&](const noisy::Event& e) {
        ctx.emit("reading", { { "sensor_id", e.payload["port"] } });
    });
}
```

Build:

```bash
g++ -std=c++20 -shared -fPIC -I<sdk-include> mysensor.cpp -o noisy_mysensor.so
```

Deploy: copy `noisy_mysensor.so` into `./modules/` or any directory listed in
`$NOISY_MODULE_PATH`. From Lua:

```lua
noisy.use("mysensor")
local info = mysensor.read("s1")
noisy.on("mysensor.reading", function(e)
    util.log("sensor " .. e.payload.sensor_id)
end)
```

Threading contract (the whole story):

- The engine delivers exactly one message at a time on a dedicated actor
  thread: `command / event / tick / reply`. No locks, no threads of your own.
- `emit / invoke_callback / call_async / reply / log / subscribe` are safe to
  call from anywhere.
- For driver-style modules, `noisy_watch(fd, interest)` parks the actor
  thread in `poll` until the descriptor is ready — no busy loop.
- A Lua function parameter arrives as a callback token; call it with
  `ctx.invoke_callback(token, payload)`.
- All data crossing the boundary is JSON. The host copies replies; nothing
  shares an allocator with the engine.
- Respond to shutdown: return from `stop` promptly; the engine joins with a
  timeout and reports stragglers as `noisy.shutdown_timeout`.

---

## 6. Applications in C++ or Rust

A complete application can be a module: the engine config names it as the
entry point, it subscribes and makes calls like any module, and shares the
bus with everything else:

```toml
# engine.toml or app config
[entry]
type = "module"
name = "myapp"
```

A Lua `app.lua` and a module app can coexist: Lua first for composition and
glue, module code for the performance-critical or C++-native parts.

---

## 7. Testing

`tests/lua/mysensor_spec.lua`:

```lua
local test   = require("test")
local events = require("event")

test.describe("mysensor", function()
    test.it("emits a reading on read()", function()
        mysensor.read("s1")
        -- event delivery is asynchronous: wait_for subscribes, then resolves
        local e = events.wait_for("mysensor.reading", 1000)
        test.expect(e).not_to_be_nil()
        test.expect(e.payload.sensor_id).to_equal("s1")
    end)
end)

test.run()
```

Rust tests live with the module crate and run with `cargo test`; C++ modules
are tested e2e by scripting them from a Lua test app through the real
engine, or with a small C++ unit harness compiled per example.

---

## 8. Document Your Module

Because the Lua surface is generated from the descriptor, documentation is a
matter of recording semantics, not signatures:

````markdown
## mysensor — My Sensor

```lua
mysensor.read(sensor_id)          -- → { read_at }
mysensor.set_interval(ms)

-- Signals
-- mysensor.reading    { sensor_id, value }
-- mysensor.error      { sensor_id, error }
```
````

Add it to `docs/LUA_API.md`.

---

## Checklist

- [ ] Descriptor declared (name, signals, methods with params, depends)
- [ ] Every method handled; `unknown_method` for the rest
- [ ] No re-entrant `call` from inside command dispatch (use `call_async`)
- [ ] Shutdown honored promptly; cleanup drains in-flight work and releases resources
- [ ] Byte payloads follow the UTF-8 / base64+`encoding` convention
- [ ] Rust: registered in `engine/src/app.rs`; C++: `.so` deployed and demonstrated from a Lua smoke test
- [ ] Rust tests in the module crate; Lua tests in `tests/lua/`; C++ e2e in `tests/cpp/`
- [ ] Documented in `docs/LUA_API.md`

---

## Stability Rules

- The descriptor is public API: adding signals or methods is a minor bump;
  changing a method's parameter order or reply shape is a breaking change.
- Module internals (state, channels, private helpers) are not public API.
- The C ABI is version-gated; see `docs/ABI.md` for its compatibility policy.
