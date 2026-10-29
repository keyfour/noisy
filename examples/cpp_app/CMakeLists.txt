# The per-module CMake scaffold (specification §6.3): one independent
# project per module — no bridge tool, no root build system.
cmake_minimum_required(VERSION 3.24)
project(noisy_myapp CXX)

set(CMAKE_CXX_STANDARD 20)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_CXX_EXTENSIONS OFF)

add_library(myapp SHARED app.cpp)
# sdk/ should be `<noisy checkout>/sdk` (or your vendored copy of it)
target_include_directories(myapp PRIVATE ${CMAKE_CURRENT_SOURCE_DIR}/../../sdk/cpp)
target_compile_options(myapp PRIVATE -Wall -Wextra -Werror)

set_target_properties(myapp PROPERTIES PREFIX "lib" OUTPUT_NAME "myapp")
