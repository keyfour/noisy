# The C ABI — Versioning and Compatibility

The single dynamic-module contract (specification §6.2) is the interface
between the engine and every `.so` module, regardless of the language it was
built in. The normative interface declaration is
`crates/noisy_ffi/include/noisy/module_c.h`; this file defines how the
version number governs it.

---

## 1. The version

- The ABI version is the integer `NOISY_ABI_VERSION` in
  `include/noisy/module_c.h`. Currently **1**.
- Every plugin exports `noisy_abi_version()`; the engine resolves it first
  and rejects the plugin when the value does not equal the engine's version
  **exactly** — there is no down-level compatibility window at load time.
  The error is a readable one-liner, e.g.  
  `ABI version mismatch in `modules/x.so`: plugin is 2, engine is 1`.
- A plugin in any language exists by mirroring the header: the same struct
  layout, the same function signatures, the same export names.

## 2. What may change without a version bump

- Fixing bugs that make the behavior match the documented contract.
- Engine-internal scheduling of the one-at-a-time delivery
  (`command / event / tick / reply`) — the ordering guarantee is only
  "one message at a time", never any stricter interleaving.
- Host-side performance: string copy strategies, dispatch plumbing.
- Anything not in the header is not ABI: engine configuration, log formats,
  registry APIs, the Lua runtime.

## 3. What requires a version bump

Bump `NOISY_ABI_VERSION` (and ship the new header) for any of:

- Changing an existing export's signature, struct layout (`noisy_host`), or
  return-value convention.
- Adding or removing a required export.
- Changing the meaning of data that crosses the boundary: the descriptor
  JSON shape, the `reply` status/error convention, the byte-payload
  convention (§3.5), or the `shutdown_flag` protocol.
- Changing the actor contract itself (locks/threads the module may use,
  when `reply()` must be called).

Per the specification's stability tiers, a breaking change bumps the
version and the previous version stays loadable for one minor cycle
(loading requires the engine to keep supporting it, not the plugin).

## 4. Conventions fixed in v1

- `int` returns: `0` = success, `<0` = failure. Positive values are
  reserved.
- `reply(status, reply_json)`: `status == 0` means success and the payload
  is `reply_json`; `status != 0` means error and `reply_json` is the
  human-readable message. Exactly one `reply()` per command.
- Asynchronous replies arrive as `noisy_module_reply(ctx, call_token,
  reply_json)`; an error reply is `{"error":"message"}`.
- All strings are NUL-terminated UTF-8; the host copies everything it
  keeps. No allocator is shared across the boundary.
- `shutdown_flag` is a C `int` pointer flipped from `0` to `1` at engine
  shutdown; read it atomically (relaxed).
- Host services may be called from anywhere **inside the module's own
  dispatched callbacks** — this includes the create-time initialization in
  `noisy_module_create`. The module never locks, never spawns threads, and
  therefore every host call happens on engine-dispatched threads.

## 5. The tick and future compatibility

- `noisy_module_tick` is an **optional** export: modules without it never
  receive ticks; modules with it receive them on a configurable interval
  (`engine.toml` [<module-name>], `tick_interval_ms`; default 1 s, `0`
  disables). The first tick fires immediately after start.
- `noisy_watch(fd, interest)` — Linux `poll` waiting for actors to await
  file descriptors — is reserved for driver modules that need it; it is not
  a host call and not part of the v1 module surface. Its declaration ships
  together with the first driver that needs it.
