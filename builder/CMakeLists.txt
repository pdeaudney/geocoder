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
