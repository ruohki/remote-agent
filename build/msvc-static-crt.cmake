# CMake toolchain file for Windows builds of C dependencies (libopus via opusic-sys).
#
# The agent links the C runtime statically (`.cargo/config.toml`, crt-static) so it starts on a
# Windows install without the VC++ redistributable. CMake >= 3.15 projects pick the MSVC runtime
# from CMAKE_MSVC_RUNTIME_LIBRARY (policy CMP0091) and ignore the /MT cmake-rs passes in the C
# flags, so the static runtime is chosen here. A cross-compiling toolchain (cargo-xwin) is
# chained in through REMOTE_AGENT_CHAIN_TOOLCHAIN.
if(DEFINED ENV{REMOTE_AGENT_CHAIN_TOOLCHAIN} AND NOT "$ENV{REMOTE_AGENT_CHAIN_TOOLCHAIN}" STREQUAL "")
  include("$ENV{REMOTE_AGENT_CHAIN_TOOLCHAIN}")
endif()
cmake_policy(SET CMP0091 NEW)
set(CMAKE_MSVC_RUNTIME_LIBRARY "MultiThreaded" CACHE STRING "" FORCE)
# libopus forces MultiThreadedDLL unless asked for its static-runtime option.
set(OPUS_STATIC_RUNTIME ON CACHE BOOL "" FORCE)
