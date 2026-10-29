#!/usr/bin/env python3
"""Module scaffold generator (module guide §1): `gen_module.py <name>
--lang rust|cpp`. Creates a minimal, buildable module skeleton.

rust: crates/modules/<name>/ registered manually in engine/src/app.rs
cpp : <out-dir>/<name>/ (default `.<name>`) with vendored SDK copies so the
      module compiles standalone; verified against the pinned vendored JSON
      header from scripts/bash/vendor_deps.sh.
"""

import argparse
import hashlib
import shutil
import sys
from pathlib import Path

NOISY_ROOT = Path(__file__).resolve().parents[2]
RUST_TEMPLATE = '''\
use noisy_core::module::Command;
use serde_json::{{json, Value}};
use tokio::task::JoinHandle;

use noisy_core::{{ModuleContext, ModuleDescriptor, ModuleError, NoisyModule}};

pub struct {struct_name};

impl NoisyModule for {struct_name} {{
    fn descriptor(&self) -> ModuleDescriptor {{
        ModuleDescriptor::new("{module}")
            .signals(["status_reported"])
            .method("status")
    }}

    fn start(self: Box<Self>, mut ctx: ModuleContext) -> JoinHandle<()> {{
        tokio::spawn(async move {{
            let this = *self;
            let shutdown = ctx.shutdown.clone();
            loop {{
                tokio::select! {{
                    maybe = ctx.commands.recv() => {{
                        let Some(cmd) = maybe else {{ break }};  // stream closed
                        match cmd {{
                            Command::MethodCall(call) => {{
                                let reply = this
                                    .dispatch(call.method(), call.params().clone(), &ctx)
                                    .await;
                                call.reply(reply);
                            }}
                            Command::AsyncReply {{ .. }} => {{}}
                        }}
                    }}
                    _ = shutdown.cancelled() => break,
                }}
            }}
            ctx.drain().await;
        }})
    }}
}}

impl {struct_name} {{
    async fn dispatch(
        &self,
        method: &str,
        _params: Value,
        ctx: &ModuleContext,
    ) -> Result<Value, ModuleError> {{
        match method {{
            "status" => Ok(json!({{ "now": ctx.host.now_ms() }})),
            _ => Err(ModuleError::unknown_method("{module}", method)),
        }}
    }}
}}
'''

RUST_CARGO = '''\
[package]
name    = "noisy_{module}"
version = "0.1.0"
edition = "2021"

[dependencies]
noisy_core = {{ path = "../../noisy_core" }}
serde_json = {{ workspace = true }}
tokio      = {{ workspace = true }}
'''

CPP_MODULE_TEMPLATE = '''\
// The `{module}` C++ module scaffolded by gen_module.py.
//
// Module state lives at file scope — the NOISY_MODULE body runs once at
// load; handlers execute on the engine's actor for the module's lifetime.
#include <noisy.hpp>

NOISY_MODULE("{module}", ctx) {{
    mod.signals = {{ "status_reported" }};
    mod.methods = {{
        {{ "status", {{ }} }}
    }};

    mod.on_command("status", [&](const nlohmann::json&) {{
        return noisy::ok({{ {{ "now", ctx.now_ms() }} }});
    }});
}}
'''

CPP_CMAKE = '''\
cmake_minimum_required(VERSION 3.24)
project(noisy_{module} CXX)

set(CMAKE_CXX_STANDARD 20)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_CXX_EXTENSIONS OFF)

add_library({module} SHARED {module}.cpp)
target_include_directories({module} PRIVATE ${{CMAKE_CURRENT_SOURCE_DIR}}/sdk)
target_compile_options({module} PRIVATE -Wall -Wextra -Werror)

set_target_properties({module} PROPERTIES PREFIX "lib" OUTPUT_NAME "{module}")
'''


def scaffold_rust(module: str, out: Path) -> None:
    crate = out / module
    (crate / "src").mkdir(parents=True)
    (crate / "Cargo.toml").write_text(RUST_CARGO.format(module=module))
    (crate / "src" / "lib.rs").write_text(
        RUST_TEMPLATE.format(struct_name=module.capitalize(), module=module)
    )
    print(f"scaffolded rust module: {crate}")
    print(f"register it in crates/engine/src/app.rs in_tree_modules()")


def scaffold_cpp(module: str, out: Path) -> None:
    module_dir = out / module
    sdk_dir = module_dir / "sdk" / "nlohmann"
    sdk_dir.mkdir(parents=True)
    (module_dir / f"{module}.cpp").write_text(
        CPP_MODULE_TEMPLATE.format(module=module)
    )
    (module_dir / "CMakeLists.txt").write_text(CPP_CMAKE.format(module=module))
    # Vendored SDK copies: the module compiles standalone, no network needed.
    shutil.copy2(NOISY_ROOT / "sdk" / "cpp" / "noisy.hpp", module_dir / "sdk" / "noisy.hpp")
    shutil.copy2(NOISY_ROOT / "sdk" / "cpp" / "nlohmann" / "json.hpp",
                 sdk_dir / "json.hpp")
    print(f"scaffolded C++ module: {module_dir}")
    print(f"build: c++ -std=c++20 -shared -fPIC -Wall -Wextra -Werror "
          f"-I{module_dir}/sdk {module_dir}/{module}.cpp "
          f"-o {module_dir}/lib{module}.so")


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(
        description="scaffold a noisy module (rust in-tree or cpp .so)")
    parser.add_argument("name", help="module name (ASCII identifier)")
    parser.add_argument("--lang", choices=("rust", "cpp"), default="rust")
    parser.add_argument("--out", type=Path, default=Path("."),
                        help="base directory (cpp scaffolds land in <out>/<name>)")
    args = parser.parse_args(argv)

    name = args.name
    if not name.isidentifier() or not name.islower():
        print(f"error: module name `{name}` must be a lowercase identifier",
              file=sys.stderr)
        return 2

    if args.lang == "rust":
        scaffold_rust(name, args.out)
    else:
        scaffold_cpp(name, args.out)
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
