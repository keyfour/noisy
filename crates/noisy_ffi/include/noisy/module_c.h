/*
 * noisy — the single dynamic-module contract (specification §6.2).
 *
 * One plugin format: a `.so` exporting exactly these symbols. The engine
 * owns one actor per dynamic module and delivers
 * `command / event / tick / reply` one at a time. Modules never lock and
 * never spawn threads. All data crosses the boundary as JSON strings;
 * byte payloads follow the §3.5 convention. The host copies all replies;
 * no allocator is shared.
 *
 * Return codes
 * ------------
 * Engine→module calls and host↔module function pointers return `int`:
 *
 *   0   success
 *  <0   the operation failed; the error is engine-side or module-side
 *       context, logged by both
 *  >0   informational (reserved; return 0 unless documented otherwise)
 *
 * `noisy_host.reply(status, reply_json)`: `status == 0` means success —
 * `reply_json` carries the reply payload; `status != 0` signals an error —
 * `reply_json` carries the human-readable error message. Exactly one
 * `reply()` per command, called on the actor thread.
 *
 * Async-call replies to a module arrive as `noisy_module_reply(ctx, id,
 * reply_json)`. Errors arrive as `reply_json == "{\"error\":\"message\"}"`.
 *
 * `shutdown_flag` points to a C int that flips from 0 to 1 at engine
 * shutdown. Read it atomically (relaxed): on C23
 * `__atomic_load_n(p, __ATOMIC_RELAXED)`, in C++ `std::atomic_ref`.
 */

#ifndef NOISY_MODULE_C_H
#define NOISY_MODULE_C_H

#include <stdint.h>

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

/* log levels — mirror noisy_core::LogLevel */
#define NOISY_LOG_DEBUG 0
#define NOISY_LOG_INFO  1
#define NOISY_LOG_WARN  2
#define NOISY_LOG_ERROR 3

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

#endif /* NOISY_MODULE_C_H */
