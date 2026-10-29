# cpp_app — a C++ application (module entry)

`myapp` is the whole application in one C++ module: the engine config
names it the entry point (`[entry] type = "module"`, specification §6.5),
the engine loads the `.so` and it runs like any module until shutdown. On
its first tick it calls `crypto.hash` through `call_async`, logs the
digest (`myapp: ready <hex>`), and emits `myapp.ready`.

## Build (one line)

```bash
scripts/bash/build_cpp_examples.sh      # builds both examples into target/cpp/
```

or, by hand (module guide §5):

```bash
c++ -std=c++20 -shared -fPIC -Wall -Wextra -Werror -Isdk/cpp \
    examples/cpp_app/app.cpp -o examples/cpp_app/modules/libmyapp.so
```

or with the per-module CMake scaffold:

```bash
cmake -S examples/cpp_app -B /tmp/opencode/cpp-app-build
cmake --build /tmp/opencode/cpp-app-build
```

## Run

```bash
mkdir -p target/e2e-cpp-app/modules
cp target/cpp/libmyapp.so target/e2e-cpp-app/modules/
cp examples/cpp_app/engine.toml target/e2e-cpp-app/
cargo run -p noisy_engine -- target/e2e-cpp-app
# ... INFO ... myapp: ready <hex> ... SIGINT to exit 0
```
