# Capture git SHA + dirty flag at build time and write them into
# `git_version.h` ONLY if the values changed. Run as a custom_target
# dependency of build-index so every `make` (or `cmake --build`)
# refreshes the SHA before compilation, instead of capturing once
# at configure time and shipping stale provenance.
#
# Caller passes:
#   -DOUTPUT=<path>     where to write git_version.h
#   -DSOURCE_DIR=<path> repo working tree (for `git -C`)

if(NOT DEFINED OUTPUT OR NOT DEFINED SOURCE_DIR)
    message(FATAL_ERROR "UpdateGitVersion.cmake requires OUTPUT and SOURCE_DIR")
endif()

find_package(Git QUIET)

set(GIT_SHA "unknown")
set(GIT_DIRTY "unknown")

if(Git_FOUND)
    execute_process(
        COMMAND ${GIT_EXECUTABLE} rev-parse --short=12 HEAD
        WORKING_DIRECTORY ${SOURCE_DIR}
        OUTPUT_VARIABLE _sha_out
        OUTPUT_STRIP_TRAILING_WHITESPACE
        ERROR_QUIET
        RESULT_VARIABLE _sha_rc
    )
    if(_sha_rc EQUAL 0 AND NOT "${_sha_out}" STREQUAL "")
        set(GIT_SHA "${_sha_out}")
    endif()

    execute_process(
        COMMAND ${GIT_EXECUTABLE} status --porcelain
        WORKING_DIRECTORY ${SOURCE_DIR}
        OUTPUT_VARIABLE _status_out
        OUTPUT_STRIP_TRAILING_WHITESPACE
        ERROR_QUIET
        RESULT_VARIABLE _status_rc
    )
    if(_status_rc EQUAL 0)
        if("${_status_out}" STREQUAL "")
            set(GIT_DIRTY "false")
        else()
            set(GIT_DIRTY "true")
        endif()
    endif()
endif()

set(NEW_CONTENT "// Auto-generated. Do not edit.\n#pragma once\n#define GEOCODER_GIT_SHA \"${GIT_SHA}\"\n#define GEOCODER_GIT_DIRTY \"${GIT_DIRTY}\"\n")

# Only rewrite the header when the values changed — otherwise every
# build rewrites the file's mtime and forces a needless recompile of
# build_index.cpp.
set(OLD_CONTENT "")
if(EXISTS "${OUTPUT}")
    file(READ "${OUTPUT}" OLD_CONTENT)
endif()

if(NOT "${OLD_CONTENT}" STREQUAL "${NEW_CONTENT}")
    file(WRITE "${OUTPUT}" "${NEW_CONTENT}")
    message(STATUS "git_version.h: sha=${GIT_SHA} dirty=${GIT_DIRTY}")
endif()
