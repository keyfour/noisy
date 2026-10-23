-- util.lua — string helpers and timestamps (noisy standard library).
-- The embedded stdlib is always available via require(); contents grow in
-- Phase 6, this seed exercises the loader.

local util = {}

function util.trim(s)
    return (s:gsub("^%s*(.-)%s*$", "%1"))
end

function util.starts_with(s, prefix)
    return s:sub(1, #prefix) == prefix
end

function util.ends_with(s, suffix)
    if #suffix == 0 then return true end
    return s:sub(-#suffix) == suffix
end

function util.split(s, sep)
    sep = sep or "%s"
    local parts = {}
    for part in string.gmatch(s, "([^" .. sep .. "]+)") do
        parts[#parts + 1] = part
    end
    return parts
end

function util.timestamp()
    return os.time() * 1000
end

return util
