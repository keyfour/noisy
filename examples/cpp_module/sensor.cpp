// The `mysensor` C++ module (module guide §5 made runnable): methods with
// structured replies, a bus slot answering a Lua event, and a tick.
//
// Module state lives at file scope — the NOISY_MODULE body runs once at
// load, then handlers execute on the engine's actor for the module's
// lifetime (sdk/cpp/noisy.hpp header comment).
#include <noisy.hpp>

static int g_readings = 0;   // served reads, surfaced through `stats`
static int g_answered = 0;   // questions the slot answered on the bus
static int g_ticks = 0;      // optional heartbeats received

NOISY_MODULE("mysensor", ctx) {
    mod.signals = { "reading", "answer" };
    mod.methods = {
        { "read",  { "sensor_id" } },
        { "stats", { } }
    };
    mod.depends = { "timer" };

    // Lua calls a C++ method and gets a structured reply back.
    mod.on_command("read", [&](const nlohmann::json& p) {
        ctx.emit("reading", { { "sensor_id", p.value("sensor_id", "s0") },
                              { "value",     42 } });
        ++g_readings;
        return noisy::ok({ { "read_at", ctx.now_ms() },
                           { "n",       g_readings } });
    });

    // The e2e polls this: reads served, questions answered, ticks seen.
    mod.on_command("stats", [&](const nlohmann::json&) {
        return noisy::ok({ { "readings", g_readings },
                           { "answered", g_answered },
                           { "ticks",    g_ticks } });
    });

    // A C++ slot receives a bus event (a Lua question) and answers back
    // onto the bus — the answer arrives in the Lua app as an event.
    bus.on("app.question", [&](const noisy::Event& e) {
        ctx.emit("answer", { { "q",      e.payload.value("q", "") },
                             { "answer", 42 } });
        ++g_answered;
    });

    // Optional heartbeat: proves the tick dispatch loop, once.
    mod.on_tick([](std::uint64_t /*now_ms*/) {
        ++g_ticks;
    });
}
