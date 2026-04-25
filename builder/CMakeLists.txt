cmake_minimum_required(VERSION 3.16)
project(traccar-geocoder-build LANGUAGES CXX)

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)

# Default to Release. Unoptimised planet builds take hours longer than
# they need to; the old implicit default was "no flags" (effectively
# -O0) on several generators.
if(NOT CMAKE_BUILD_TYPE)
    set(CMAKE_BUILD_TYPE Release CACHE STRING "" FORCE)
endif()

# The on-disk format is native-endian; runtime readers (Rust) assume
# little-endian. Refuse to produce indexes on a big-endian host rather
# than silently ship unreadable output.
include(TestBigEndian)
test_big_endian(HOST_IS_BIG_ENDIAN)
if(HOST_IS_BIG_ENDIAN)
    message(FATAL_ERROR "traccar-geocoder index files are little-endian only; builder host is big-endian")
endif()

find_package(Threads REQUIRED)
find_package(ZLIB REQUIRED)
find_package(BZip2 REQUIRED)
find_package(EXPAT REQUIRED)
find_package(s2 QUIET)

add_executable(build-index src/build_index.cpp)

# Vendored ankerl::unordered_dense (single-header, MIT-licensed) at
# third_party/ankerl/. Replaces std::unordered_map across the
# builder's ingestion path: dense flat storage, separate-payload
# layout that handles the `vector<u32>` value type cheaply on
# rehash, and best-in-class iteration which the write_index flush
# leans on.
#
# See docs/performance/hashmap-choice-2026-04-25.md for the
# benchmark references and analysis behind this choice. Pinned to
# v4.8.1; refresh by re-vendoring upstream.
target_include_directories(build-index PRIVATE third_party)

# Warnings + optimisation. -Wconversion in particular catches the
# narrowing size_t → uint32_t casts the on-disk format is sensitive
# to; we leave it off by default and let specific call sites assert
# bounds (see uint32 overflow guards in build_index.cpp).
target_compile_options(build-index PRIVATE
    -Wall -Wextra -Wshadow -Wpedantic
)
target_compile_options(build-index PRIVATE
    $<$<CONFIG:Release>:-O3 -DNDEBUG>
)

# Per-arch tuning for release builds. Matches .cargo/config.toml — we
# deploy on AWS x86_64 (Haswell+) and Graviton 4+. On Linux pick the
# right flag; on macOS leave the native default alone (dev laptops,
# not production).
if(CMAKE_SYSTEM_NAME STREQUAL "Linux")
    if(CMAKE_SYSTEM_PROCESSOR MATCHES "x86_64|AMD64")
        target_compile_options(build-index PRIVATE
            $<$<CONFIG:Release>:-march=x86-64-v3>
        )
    elseif(CMAKE_SYSTEM_PROCESSOR MATCHES "aarch64|arm64")
        target_compile_options(build-index PRIVATE
            $<$<CONFIG:Release>:-mcpu=neoverse-v2>
        )
    endif()
endif()

if(EXISTS /opt/homebrew/include AND APPLE)
    target_include_directories(build-index PRIVATE /opt/homebrew/include)
endif()

if(s2_FOUND)
    target_link_libraries(build-index PRIVATE
        s2::s2
        ZLIB::ZLIB
        BZip2::BZip2
        EXPAT::EXPAT
        Threads::Threads
    )
else()
    target_include_directories(build-index PRIVATE /usr/include/s2)
    target_link_libraries(build-index PRIVATE
        s2
        ZLIB::ZLIB
        BZip2::BZip2
        EXPAT::EXPAT
        Threads::Threads
    )
endif()
