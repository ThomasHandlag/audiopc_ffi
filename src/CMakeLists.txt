cmake_minimum_required(VERSION 3.10)

# 1. Add CXX (C++) to the supported languages
project(audiopc_ffi_library VERSION 0.0.1 LANGUAGES C)

add_library(audiopc_ffi SHARED
  "audiopc_ffi.c"
)

set_target_properties(audiopc_ffi PROPERTIES
  PUBLIC_HEADER audiopc_ffi.h
  OUTPUT_NAME "audiopc_ffi"
)

target_compile_definitions(audiopc_ffi PUBLIC DART_SHARED_LIB)

if (ANDROID)
  # Support Android 15 16k page size
  target_link_options(audiopc_ffi PRIVATE "-Wl,-z,max-page-size=16384")
endif()