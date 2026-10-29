-- The cpp_module e2e app (acceptance §Phase 5): Lua calls a C++ method
-- and gets a structured reply; a C++ slot receives a Lua bus event and
-- answers back onto the bus. Every assertion failure aborts the script,
-- which fails the engine; `cpp_module: e2e ok` is the success marker.
local function wait_until(condition)
    local attempts = 0
    while not condition() do
        attempts = attempts + 1
        assert(attempts < 100000, "condition never became true")
        timer.sleep(10)   -- parks this Lua job; the bus keeps moving
    end
end

-- 1. C++ method call -> structured reply
local reply = mysensor.read("s1")
assert(reply.n == 1, "readings counter: expected 1, got " .. tostring(reply.n))
assert(reply.read_at ~= nil, "read_at must carry the engine clock")

-- 2. C++ slot receives the event and answers back into Lua
noisy.emit("app.question", { q = "meaning" })
wait_until(function()
    return mysensor.stats().answered > 0
end)

-- 3. the optional tick heartbeat reached the C++ module
wait_until(function()
    return mysensor.stats().ticks > 0
end)

print("cpp_module: e2e ok")
