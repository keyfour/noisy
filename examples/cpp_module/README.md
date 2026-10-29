# cpp_module — a C++ module + Lua application

`mysensor` is a C++ `.so` module built with `sdk/cpp/noisy.hpp`; `app.lua`
is the application driving it end to end:

1. Lua calls the C++ method `read` and gets a structured reply.
2. Lua emits `app.question`, the C++ slot answers back with `mysensor.answer`.
3. The optional tick heartbeat reaches the module (`stats.ticks > 0`).

## Build (one line)

```bash
scripts/bash/build_cpp_examples.sh      # builds both examples into target/cpp/
```

or, by hand (module guide §5):

```bash
c++ -std=c++20 -shared -fPIC -Wall -Wextra -Werror -Isdk/cpp \
    examples/cpp_module/sensor.cpp -o examples/cpp_module/modules/libmysensor.so
```

or with the per-module CMake scaffold:

```bash
cmake -S examples/cpp_module -B /tmp/opencode/cpp-module-build
cmake --build /tmp/opencode/cpp-module-build
```

## Run

The engine searches `<app-dir>/modules/` by default:

```bash
mkdir -p target/e2e-cpp-module/modules
cp target/cpp/libmysensor.so target/tmp/e2e-cpp-module/modules/
cp examples/cpp_module/{app.lua,engine.toml} target/e2e-cpp-module/
cargo run -p noisy_engine -- target/e2e-cpp-module
# ... INFO ... mysensor ... and `cpp_module: e2e ok`, SIGINT to exit
```
