// noisy — the header-only C++ SDK (specification §6.3, module guide §5).
//
// A C++ module is a `.so` exporting the single dynamic-module contract of
// `crates/noisy_ffi/include/noisy/module_c.h`; this header wraps that
// contract so a module reads like the rest of noisy:
//
//     #include <noisy.hpp>
//
//     NOISY_MODULE("mysensor", ctx) {
//         mod.signals = { "reading", "error" };
//         mod.methods = { { "read", { "sensor_id" } } };
//
//         mod.on_command("read", [&](const nlohmann::json& p) {
//             ctx.emit("reading", { { "sensor_id", p["sensor_id"] } });
//             return noisy::ok({ { "read_at", ctx.now_ms() } });
//         });
//
//         bus.on("serial.data_received", [&](const noisy::Event& e) {
//             ctx.log(NOISY_LOG_INFO, "frame", { { "bytes", 16 } });
//         });
//     }
//
// The macro declares three objects in the module body: `mod` (the
// descriptor + handler registry), `bus` (slot connections), and the object
// the macro's second argument names (`ctx` above) — the module's host
// services. Exactly one NOISY_MODULE per `.so`; the compile fails loudly
// on two, which is the right place to catch it.
//
// Contract mapping (docs/ABI.md):
//   - module state     → one static `noisy::state`, returned from create
//   - descriptor       → frozen from `mod.*` at load, before any host call
//   - commands         → host.reply(0, json) exactly once per command;
//                        handler exceptions surface as the error reply
//   - events           → delivered to every matching `bus.on` slot
//   - async replies    → `ctx.call_async(..., on_reply)` correlation by id
//   - tick / stop      → optional via `mod.on_tick` / `mod.on_stop`
//   - shutdown_flag    → `ctx.shutting_down()` (atomic ref, relaxed)
//
// Host services are only valid while the engine is dispatching — the
// actor busy loop calls command / event / tick / reply one at a time and
// the contract forbids locking or extra threads, so the SDK never locks.
// Modules may however park in `poll`-style readiness paths per ABI.md.
//
// JSON: handler signatures speak `nlohmann::json` (specification §7
// dependency table); the vendored single-header lives at
// `sdk/cpp/nlohmann/json.hpp`, pinned through `scripts/bash/vendor_deps.sh`.

#ifndef NOISY_HPP_INCLUDED
#define NOISY_HPP_INCLUDED

#include "nlohmann/json.hpp"

#include <atomic>
#include <cstdint>
#include <exception>
#include <functional>
#include <stdexcept>
#include <string>
#include <utility>
#include <vector>
#include <unordered_map>

// Opaque engine-side module context, at global scope exactly as the C
// ABI declares it (module_c.h). The SDK never dereferences it.
struct noisy_ctx;

namespace noisy {

// ────────────────────── the C ABI, mirrored ──────────────────────────

// Opaque engine-side module context — declared at global scope so the
// SDK speaks the same wire shape as `module_c.h` (its comments live at
// the top of this header). The SDK never dereferences it.

#ifndef NOISY_ABI_VERSION
#define NOISY_ABI_VERSION 1
#endif

#ifndef NOISY_LOG_DEBUG
#define NOISY_LOG_DEBUG 0
#define NOISY_LOG_INFO  1
#define NOISY_LOG_WARN  2
#define NOISY_LOG_ERROR 3
#endif

// The mirror structure follows the semantics of the declarations below.

/// The engine's function table — layout-identical to `noisy_host` in
/// `module_c.h` so the struct can cross the C boundary by value.
struct host {
    int  (*emit)(::noisy_ctx*, const char* kind, const char* payload_json);
    int  (*invoke_callback)(::noisy_ctx*, std::uint64_t token,
                            const char* payload_json);
    int  (*subscribe)(::noisy_ctx*, const char* pattern);
    int  (*call_async)(::noisy_ctx*, const char* module, const char* method,
                       const char* params_json);
    int  (*reply)(::noisy_ctx*, int status, const char* reply_json);
    int  (*log)(::noisy_ctx*, int level, const char* msg,
                const char* fields_json);
    std::uint64_t (*now_ms)(::noisy_ctx*);
    int* shutdown_flag;
};

/// A handler failure: throws out of `on_command` and surfaces as the
/// command's error reply (status != 0, the message as the reply body).
class error : public std::runtime_error {
public:
    using std::runtime_error::runtime_error;
};

/// One delivered bus event (`{module}.{kind}`, payload as JSON).
struct Event {
    std::string name;             // full bus name, e.g. "app.question"
    nlohmann::json payload;
};

// Handler types (see `module` / `bus` below).
using command_fn = std::function<nlohmann::json(const nlohmann::json&)>;  // reply payload
using slot_fn    = std::function<void(const Event&)>;                    // void; throw → logged
using reply_fn   = std::function<void(std::uint64_t token,
                                      const nlohmann::json& value)>;
using tick_fn    = std::function<void(std::uint64_t now_ms)>;
using stop_fn    = std::function<void(std::uint64_t timeout_ms)>;

// ─────────────────────────── module state ────────────────────────────

struct state;

// Internal bus-pattern rule (mirrors noisy_core's event_matches):
//   "*" — everything; "app." — every event of `app`; "app.ready" — exact.
inline bool pattern_matches(const std::string& pattern, const std::string& name,
                            const std::string& source) {
    if (pattern == "*") {
        return true;
    }
    if (pattern.size() > 1 && pattern.back() == '.') {
        std::string prefix = pattern.substr(0, pattern.size() - 1);
        return source == prefix;
    }
    return pattern == name;
}

struct state {
    explicit state(std::string name) : name(std::move(name)) {}

    // Descriptor pieces (filled by the module body through `module`).
    std::string name;
    std::string version = "0.1.0";
    std::vector<std::string> signals;
    std::vector<std::string> depends;
    struct method {
        std::string name;
        std::vector<std::string> params;
    };
    std::vector<method> methods;
    std::string desc;                        // frozen descriptor JSON

    // Handler registry.
    std::unordered_map<std::string, command_fn> commands;
    std::vector<std::pair<std::string, slot_fn>> slots;
    std::unordered_map<std::uint64_t, reply_fn> pending;
    tick_fn on_tick;
    stop_fn on_stop;

    // Live engine link, set inside create.
    host table{};
    bool connected = false;
    ::noisy_ctx* ctx = nullptr;

    /// Stores the engine's table (a copy — no shared allocator), then
    /// connects every `bus.on` slot. Runs inside the engine's actor
    /// dispatch, so the host calls are on the contract's happy path.
    void connect(const host& engine) {
        table = engine;
        connected = true;
        ctx = reinterpret_cast<::noisy_ctx*>(this);
        for (auto& entry : slots) {
            table.subscribe(ctx, entry.first.c_str());
        }
    }

    void reply_text(int status, const std::string& text) const {
        table.reply(ctx, status, text.c_str());
    }

    void dispatch_log(int level, const std::string& msg) {
        if (table.log == nullptr) {
            return;
        }
        const nlohmann::json fields = nlohmann::json::object();
        const std::string fields_text = fields.dump();
        table.log(ctx, level, msg.c_str(), fields_text.c_str());
    }

    bool dispatch_command(const char* name, const char* params_json) {
        const std::string method = name == nullptr ? "" : name;
        const auto found = commands.find(method);
        if (found == commands.end()) {
            reply_text(1, std::string("unknown method `") + method
                              + "` (module serves: " + name_of() + ")");
            return false;
        }
        nlohmann::json params;
        try {
            params = nlohmann::json::parse(
                params_json == nullptr ? "{}" : params_json);
        } catch (const nlohmann::json::exception&) {
            reply_text(1, std::string("bad params JSON on `") + method + "`");
            return false;
        }
        try {
            const nlohmann::json value = found->second(params);
            reply_text(0, value.dump());
        } catch (const nlohmann::json::exception& e) {
            reply_text(1, std::string("reply serialization failed: ") + e.what());
        } catch (const std::exception& e) {
            reply_text(1, e.what());
        } catch (...) {
            reply_text(1, "unknown C++ exception");
        }
        return true;
    }

    void dispatch_event(const char* name, const char* payload_json) {
        Event event;
        event.name = name == nullptr ? "" : name;
        try {
            event.payload = nlohmann::json::parse(
                payload_json == nullptr ? "{}" : payload_json);
        } catch (const nlohmann::json::exception&) {
            return;   // undeliverable payload; nothing sensible to route
        }
        const std::size_t dot = event.name.rfind('.');
        const std::string source = dot == std::string::npos
                                       ? event.name
                                       : event.name.substr(0, dot);
        for (auto& entry : slots) {
            if (!pattern_matches(entry.first, event.name, source) ||
                entry.second == nullptr) {
                continue;
            }
            try {
                entry.second(event);
            } catch (const std::exception& e) {
                // Protect the actor: log the failure, keep serving.
                dispatch_log(NOISY_LOG_ERROR,
                             std::string("slot on `") + event.name
                                 + "` failed: " + e.what());
            } catch (...) {
                dispatch_log(NOISY_LOG_ERROR,
                             std::string("slot on `") + event.name
                                 + "` raised an unknown exception");
            }
        }
    }

    void dispatch_tick(std::uint64_t now_ms_value) {
        if (on_tick == nullptr) return;
        try {
            on_tick(now_ms_value);
        } catch (const std::exception& e) {
            dispatch_log(NOISY_LOG_ERROR,
                         std::string("tick failed: ") + e.what());
        } catch (...) {
            dispatch_log(NOISY_LOG_ERROR, "tick raised an unknown exception");
        }
    }

    void dispatch_reply(std::uint64_t token, const char* reply_json) {
        const auto found = pending.find(token);
        if (found == pending.end()) {
            try {
                dispatch_log(
                    2, "async reply for unknown call token "
                           + std::to_string(token) + " (handler not registered)");
            } catch (...) {
            }
            return;
        }
        reply_fn on_reply = std::move(found->second);
        pending.erase(found);
        nlohmann::json value;
        try {
            value = nlohmann::json::parse(
                reply_json == nullptr ? "{}" : reply_json, nullptr, false);
            if (value.is_discarded()) {
                value = nlohmann::json::object();
                dispatch_log(NOISY_LOG_WARN,
                             "async reply was not valid JSON; delivered empty");
            }
        } catch (...) {
            value = nlohmann::json::object();
        }
        try {
            on_reply(token, value);
        } catch (const std::exception& e) {
            dispatch_log(NOISY_LOG_ERROR,
                         std::string("reply handler failed: ") + e.what());
        } catch (...) {
            dispatch_log(NOISY_LOG_ERROR,
                         "reply handler raised an unknown exception");
        }
    }

    void dispatch_stop(std::uint64_t timeout_ms_value) {
        if (on_stop == nullptr) return;
        try {
            on_stop(timeout_ms_value);
        } catch (const std::exception& e) {
            dispatch_log(NOISY_LOG_ERROR,
                         std::string("stop handler failed: ") + e.what());
        } catch (...) {
            dispatch_log(NOISY_LOG_ERROR,
                         "stop handler raised an unknown exception");
        }
    }

    void teardown() {
        commands.clear();
        slots.clear();
        pending.clear();
        on_tick = nullptr;
        on_stop = nullptr;
        connected = false;
    }

private:
    std::string name_of() const {
        std::string served;
        for (const auto& entry : commands) {
            if (!served.empty()) served += ", ";
            served += entry.first;
        }
        return served;
    }
};

// ───────────────────── descriptor + ok() helper ──────────────────────

/// Handlers return the reply payload; `noisy::ok(...)` is documentation
/// on top — makes the happy path read as a value, not plumbing (module
/// guide §5 uses it in examples).
inline nlohmann::json ok(nlohmann::json value) {
    return value;
}

// ─────────────────────── module / bus / ctx views ────────────────────

/// Descriptor pieces + handler registration. Unused methods reply
/// `unknown method` (the service contract: every declared method is
/// handled; unknown_method for the rest — module guide checklist).
class module {
public:
    explicit module(state* st) : st_(st) {}

    std::vector<std::string> signals;
    std::vector<std::string> depends;
    struct method {
        std::string name;
        std::vector<std::string> params;
    };
    std::vector<method> methods;

    /// Registers the handler for a declared method. Its return value is
    /// the reply payload; throwing `noisy::error` (or anything derived
    /// from `std::exception`) makes the calling side get an error whose
    /// message is `what()`. Exactly one reply per command, by contract.
    template <class Fn>
    void on_command(const std::string& method_name, Fn&& fn) {
        st_->commands[method_name] = std::forward<Fn>(fn);
    }

    /// Optional heartbeat — fires on the actor only when the module's
    /// `tick_interval_ms` engine knob is > 0 (the default is 1000).
    template <class Fn>
    void on_tick(Fn&& fn) {
        st_->on_tick = std::forward<Fn>(fn);
    }

    /// Optional cleanup; the engine gives `stop` the shutdown join budget.
    template <class Fn>
    void on_stop(Fn&& fn) {
        st_->on_stop = std::forward<Fn>(fn);
    }

    /// Copies the view's descriptor pieces into module state; the macro
    /// calls it once the module body has run.
    void sync_to_state() {
        st_->signals = signals;
        st_->depends = depends;
        st_->methods.clear();
        st_->methods.reserve(methods.size());
        for (const method& m : methods) {
            st_->methods.push_back(state::method{ m.name, m.params });
        }
    }

private:
    state* st_;
};

/// Slot connections. Registration happens whenever the module body runs
/// (a pure data operation); the engine's `subscribe` call is deferred
/// until create — bus events only flow once the module is loaded anyway.
class bus {
public:
    explicit bus(state* st) : st_(st) {}

    /// Connects a slot. Pattern shapes (mirroring the engine):
    ///   "*"           — every event on the bus
    ///   "net."        — every `net.*` event
    ///   "net.ready"   — exactly that event
    template <class Fn>
    void on(const std::string& pattern, Fn&& fn) {
        st_->slots.emplace_back(pattern, std::forward<Fn>(fn));
    }

private:
    state* st_;
};

/// The module's host services (the `ctx` named in NOISY_MODULE).
/// Calls are valid on the actor — inside command / event / tick /
/// reply handlers and create — and nowhere else.
class service {
public:
    explicit service(state* st) : st_(st) {}

    /// Publishes `{module}.{kind}` with the payload on the bus
    /// (fire-and-forget; matching slots are fed asynchronously).
    void emit(const std::string& kind, const nlohmann::json& payload) const {
        require_connected();
        const std::string payload_text = payload.dump();
        st_->table.emit(st_->ctx, kind.c_str(), payload_text.c_str());
    }

    /// Fires an asynchronous command (`module.method`, JSON params).
    /// When a reply handler is given it is invoked on the actor when the
    /// reply arrives — `noisy_module_reply` carries errors as
    /// `{ "error": "message" }`. Returns the call token for correlation.
    std::uint64_t call_async(const std::string& target,
                             const std::string& method_name,
                             const nlohmann::json& params_json,
                             reply_fn on_reply = nullptr) const {
        require_connected();
        const std::string params_text = params_json.dump();
        const int id = st_->table.call_async(st_->ctx, target.c_str(),
                                             method_name.c_str(),
                                             params_text.c_str());
        if (id < 0) {
            throw error("call_async() was rejected by the host (is `"
                        + target + "` loaded?)");
        }
        if (on_reply != nullptr) {
            st_->pending.emplace(static_cast<std::uint64_t>(id),
                                 std::move(on_reply));
        }
        return static_cast<std::uint64_t>(id);
    }

    /// Structured log entry (mirrors onto the bus as `log.entry`).
    /// Levels: NOISY_LOG_DEBUG / INFO / WARN / ERROR.
    void log(int level, const std::string& msg,
             const nlohmann::json& fields = nlohmann::json::object()) const {
        require_connected();
        const std::string fields_text = fields.dump();
        st_->table.log(st_->ctx, level, msg.c_str(), fields_text.c_str());
    }

    /// Epoch milliseconds, engine clock.
    std::uint64_t now_ms() const {
        require_connected();
        return st_->table.now_ms(st_->ctx);
    }

    /// True after the engine flipped the shutdown flag (0 → 1).
    bool shutting_down() const {
        if (st_->table.shutdown_flag == nullptr) {
            return false;
        }
        // The contract's read rule: relaxed, one-word.
        return std::atomic_ref<int>(*st_->table.shutdown_flag)
                   .load(std::memory_order_relaxed) != 0;
    }

private:
    state* st_;

    void require_connected() const {
        if (!st_->connected) {
            throw error("host services are unavailable outside the actor "
                        "(before create returned)");
        }
    }
};

// Freezes the descriptor after the module body ran.
inline void seal(state& st) {
    nlohmann::json json = nlohmann::json::object();
    json["name"] = st.name;
    json["version"] = st.version;
    json["signals"] = st.signals;
    nlohmann::json json_methods = nlohmann::json::array();
    for (const state::method& m : st.methods) {
        nlohmann::json entry = nlohmann::json::object();
        entry["name"] = m.name;
        entry["params"] = m.params;
        json_methods.push_back(entry);
    }
    json["methods"] = json_methods;
    json["depends"] = st.depends;
    st.desc = json.dump();
}

}  // namespace noisy

// ────────────────────────── NOISY_MODULE ─────────────────────────────

// One module per `.so`: the plumbing is file-scope static so the second
// macro use in one translation unit is a loud compile error.
//
// Expansion order maps the loader's calls:
//   1. `noisy_abi_version` / `noisy_module_descriptor` run the module body
//      once — descriptor only needs registry data, no host calls.
//   2. `noisy_module_create` stores the engine table and connects the
//      `bus.on` slots (valid to call as part of create's dispatch).
//   3. command / event / tick / reply / stop / destroy dispatch through it.
#define NOISY_MODULE(_module_name, _ctx_name)                                     \
    static void noisy_cpp_sdk_body([[maybe_unused]] noisy::module& mod,          \
                                   [[maybe_unused]] noisy::bus& bus,             \
                                   noisy::service& _ctx_name);                   \
    static noisy::state& noisy_cpp_sdk_state() {                                  \
        static noisy::state s(_module_name);                                      \
        return s;                                                                 \
    }                                                                             \
    static void noisy_cpp_sdk_boot() {                                            \
        static bool done = false;                                                 \
        if (done) {                                                               \
            return;                                                               \
        }                                                                         \
        done = true;                                                              \
        noisy::state& s = noisy_cpp_sdk_state();                                  \
        /* static: handlers capture these by reference and run long after */      \
        /* the body returned — module state lives at file scope anyway. */      \
        static noisy::module mod(&s);                                             \
        static noisy::bus bus(&s);                                                \
        static noisy::service _ctx_name(&s);                                      \
        noisy_cpp_sdk_body(mod, bus, _ctx_name);                                  \
        mod.sync_to_state();                                                      \
        noisy::seal(s);                                                           \
    }                                                                             \
    extern "C" const char* noisy_abi_version(void) {                              \
        return "1";                                                               \
    }                                                                             \
    extern "C" const char* noisy_module_descriptor(void) {                        \
        noisy_cpp_sdk_boot();                                                     \
        return noisy_cpp_sdk_state().desc.c_str();                                \
    }                                                                             \
    extern "C" ::noisy_ctx* noisy_module_create(noisy::host engine_table) {         \
        noisy_cpp_sdk_boot();                                                     \
        noisy_cpp_sdk_state().connect(engine_table);                              \
        return noisy_cpp_sdk_state().ctx;                                         \
    }                                                                             \
    extern "C" int noisy_module_command(::noisy_ctx* module_ctx,                    \
                                        const char* method,                       \
                                        const char* params_json) {                \
        noisy_cpp_sdk_state().dispatch_command(method, params_json);              \
        (void)module_ctx;                                                         \
        return 0;                                                                 \
    }                                                                             \
    extern "C" int noisy_module_tick(::noisy_ctx* module_ctx,                       \
                                     std::uint64_t now_ms_value) {                \
        noisy_cpp_sdk_state().dispatch_tick(now_ms_value);                        \
        (void)module_ctx;                                                         \
        return 0;                                                                 \
    }                                                                             \
    extern "C" int noisy_module_event(::noisy_ctx* module_ctx, const char* name,    \
                                      const char* payload_json) {                 \
        noisy_cpp_sdk_state().dispatch_event(name, payload_json);                 \
        (void)module_ctx;                                                         \
        return 0;                                                                 \
    }                                                                             \
    extern "C" int noisy_module_reply(::noisy_ctx* module_ctx,                      \
                                      std::uint64_t call_token,                   \
                                      const char* reply_json) {                   \
        noisy_cpp_sdk_state().dispatch_reply(call_token, reply_json);             \
        (void)module_ctx;                                                         \
        return 0;                                                                 \
    }                                                                             \
    extern "C" int noisy_module_stop(::noisy_ctx* module_ctx,                       \
                                     std::uint64_t timeout_ms) {                  \
        noisy_cpp_sdk_state().dispatch_stop(timeout_ms);                          \
        (void)module_ctx;                                                         \
        return 0;                                                                 \
    }                                                                             \
    extern "C" void noisy_module_destroy(::noisy_ctx* module_ctx) {                 \
        noisy_cpp_sdk_state().teardown();                                         \
        (void)module_ctx;                                                         \
    }                                                                             \
    static void noisy_cpp_sdk_body([[maybe_unused]] noisy::module& mod,          \
                                   [[maybe_unused]] noisy::bus& bus,             \
                                   noisy::service& _ctx_name)

#endif  // NOISY_HPP_INCLUDED
