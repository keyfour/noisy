// The `myapp` C++ application (specification §6.5, module guide §6): a
// whole application as one module — the engine config names it the entry
// (`[entry] type = "module"`), the engine loads it, and it runs like any
// module until shutdown.
//
// The application body runs on the module's first tick (the engine fires
// the initial heartbeat immediately, then every `tick_interval_ms`): it
// calls another module through `call_async`, logs the digest, and goes
// back to sleep — the e2e greps the log line from stdout.
#include <noisy.hpp>

static bool g_started = false;   // file scope: the one-shot application body

NOISY_MODULE("myapp", ctx) {
    mod.signals = { "ready" };
    mod.methods = {
        { "status", { } }
    };

    mod.on_command("status", [&](const nlohmann::json&) {
        return noisy::ok({ { "started", g_started } });
    });

    mod.on_tick([&](std::uint64_t /*now_ms*/) {
        if (g_started or ctx.shutting_down()) {
            return;
        }
        g_started = true;   // ticks keep firing; run the body exactly once

        ctx.log(NOISY_LOG_INFO, "myapp: startup — hashing through crypto");

        // The engine reports async-call errors as { "error": "message" };
        // `crypto.hash` replies with the hex digest string.
        ctx.call_async("crypto", "hash",
                       { { "algorithm", "sha256" }, { "data", "noisy" } },
                       [&ctx](std::uint64_t, const nlohmann::json& value) {
                           if (value.contains("error")) {
                               ctx.log(NOISY_LOG_ERROR,
                                       "myapp: crypto call failed — "
                                           + value["error"].dump());
                               return;
                           }
                           ctx.log(NOISY_LOG_INFO,
                                   "myapp: ready " + value.get<std::string>());
                           ctx.emit("ready", { { "digest", value } });
                       });
    });
}
