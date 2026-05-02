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

# libdeflate replaces zlib for PBF inflate when available.
# 2-3× faster on the same data (libdeflate uses SSE4.2/AVX2 SIMD
# for the Huffman + LZ77 fast paths). libosmium picks it up at
# compile time via the `OSMIUM_WITH_LIBDEFLATE` macro defined
# below — see docs/performance/build-pipeline-perf-plan.md
# stage 1 for analysis.
find_library(LIBDEFLATE_LIBRARY deflate
    PATHS /opt/homebrew/lib /usr/local/lib /usr/lib
    DOC "libdeflate library (for fast PBF inflate)")
find_path(LIBDEFLATE_INCLUDE_DIR libdeflate.h
    PATHS /opt/homebrew/include /usr/local/include /usr/include
    DOC "libdeflate header")

add_executable(build-index src/build_index.cpp)

# Git provenance — refreshed at every build via a custom_target that
# regenerates `git_version.h` before build-index compiles. Capturing
# at configure time (the previous approach) shipped a stale SHA when
# operators ran `make builder` after a new commit without re-running
# cmake; that defeated the manifest's whole purpose. The CMake script
# at cmake/UpdateGitVersion.cmake only rewrites the header when the
# values actually changed, so steady-state recompiles are unaffected.
set(GIT_VERSION_HEADER "${CMAKE_CURRENT_BINARY_DIR}/git_version.h")
add_custom_target(update_git_version ALL
    BYPRODUCTS "${GIT_VERSION_HEADER}"
    COMMAND ${CMAKE_COMMAND}
        -DOUTPUT=${GIT_VERSION_HEADER}
        -DSOURCE_DIR=${CMAKE_CURRENT_SOURCE_DIR}
        -P ${CMAKE_CURRENT_SOURCE_DIR}/cmake/UpdateGitVersion.cmake
    COMMENT "Updating git_version.h"
    VERBATIM
)
add_dependencies(build-index update_git_version)
target_include_directories(build-index PRIVATE "${CMAKE_CURRENT_BINARY_DIR}")

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

# GCC-only false-positive suppressions. Both fire on upstream code
# (S2 headers, libosmium) and have nothing to do with our build_index.cpp.
# Gated on compiler id so clang/AppleClang on dev macOS is unaffected.
if(CMAKE_CXX_COMPILER_ID STREQUAL "GNU")
    # `-Wno-psabi`: GCC 10.1+ emits a *note* (informational, not a
    # warning) "parameter passing for argument of type X when C++17
    # is enabled changed to match C++14 in GCC 10.1" on every
    # small-struct-by-value site in the S2 headers on aarch64. ~30+
    # notes per build for zero signal — describes an ABI change
    # between compiler versions, no codegen impact, no risk of
    # hiding a real bug.
    #
    # `-Wno-stringop-overread`: GCC 11+ emits a false positive in
    # libosmium's `OSMObjectBuilder::set_user` — the static analyser
    # can't follow osmium's tagged-pointer / computed-offset pattern
    # used to lay out variable-length user names after the fixed-size
    # Relation struct, and concludes "offset [34, 42] into source
    # object of size 32" must be out-of-bounds. It isn't; osmium's
    # allocator extends the buffer past the struct's nominal size on
    # purpose. The warning fires only after several inlining passes
    # so a #pragma at the include site doesn't catch it. Suppression
    # is upstream-recommended for this specific GCC + osmium combo.
    target_compile_options(build-index PRIVATE -Wno-psabi -Wno-stringop-overread)
endif()
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

# Link libdeflate when found. The OSMIUM_WITH_LIBDEFLATE define
# tells libosmium to use it for PBF block inflate; without the
# define libosmium silently falls back to zlib.
if(LIBDEFLATE_LIBRARY AND LIBDEFLATE_INCLUDE_DIR)
    message(STATUS "build-index: using libdeflate (${LIBDEFLATE_LIBRARY})")
    target_compile_definitions(build-index PRIVATE OSMIUM_WITH_LIBDEFLATE)
    target_include_directories(build-index PRIVATE ${LIBDEFLATE_INCLUDE_DIR})
    target_link_libraries(build-index PRIVATE ${LIBDEFLATE_LIBRARY})
else()
    message(STATUS "build-index: libdeflate not found, falling back to zlib (slower PBF inflate)")
endif()
