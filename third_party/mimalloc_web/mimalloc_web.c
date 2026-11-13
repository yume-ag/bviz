/* ----------------------------------------------------------------------------
Copyright (c) 2018-2023, Microsoft Research, Daan Leijen
This is free software; you can redistribute it and/or modify it under the
terms of the MIT license. A copy of the license is mimalloc's LICENSE file:
https://github.com/microsoft/mimalloc/blob/v3.3.2/LICENSE
-----------------------------------------------------------------------------*/

// mimalloc for a browser with threads: the platform-independent sources
// src/static.c includes, less the POSIX entry points of alloc-posix.c, and
// primitives adapted from src/prim/wasi/prim.c that take memory from the heap
// bviz_alloc shares between threads, with no C library behind them.

// The memory has no virtual reserve, so each arena is taken whole from the
// shared heap: mimalloc reserves a quarter of this, its minimum arena size.
#define MI_DEFAULT_ARENA_RESERVE (32 * 1024)

// The largest arena, past which mimalloc's reservations, doubling every eight
// arenas, stop growing: the smallest power of two at which the arenas mimalloc
// may reserve cover the memory.
#define MI_ARENA_MAX_RESERVE ((size_t)32 << 20)

#include "mimalloc.h"
#include "mimalloc/internal.h"
#include "mimalloc/prim.h"

#include "alloc.c"
#include "alloc-aligned.c"
#include "arena.c"
#include "arena-meta.c"
#include "bitmap.c"
#include "heap.c"
#include "init.c"
#include "libc.c"
#include "options.c"
#include "os.c"
#include "page.c"
#include "page-map.c"
#include "random.c"
#include "stats.c"
#include "theap.c"
#include "threadlocal.c"

_Thread_local int errno;

#define WASM_PAGE_SIZE (64 * MI_KiB)

// Initialize

void _mi_prim_mem_init(mi_os_mem_config_t* config) {
  config->page_size = WASM_PAGE_SIZE;
  config->alloc_granularity = WASM_PAGE_SIZE;
  config->has_overcommit = false;
  config->has_partial_free = false;
  config->has_virtual_reserve = false;
}

// Memory: regions of bviz_alloc's shared heap, which are usable until given
// back, so committing, decommitting and resetting them does nothing.

void* bviz_mimalloc_os_alloc(size_t size, size_t align);
void bviz_mimalloc_os_free(void* region, size_t size);

int _mi_prim_free(void* addr, size_t size) {
  bviz_mimalloc_os_free(addr, size);
  return 0;
}

int _mi_prim_alloc(void* hint_addr, size_t size, size_t try_alignment, bool commit, bool allow_large,
                   bool* is_large, bool* is_zero, void** addr) {
  MI_UNUSED(hint_addr);
  MI_UNUSED(commit);
  MI_UNUSED(allow_large);
  *is_large = false;
  *is_zero = false;
  *addr = bviz_mimalloc_os_alloc(size, try_alignment);
  return (*addr != NULL ? 0 : ENOMEM);
}

int _mi_prim_commit(void* addr, size_t size, bool* is_zero) {
  MI_UNUSED(addr);
  MI_UNUSED(size);
  *is_zero = false;
  return 0;
}

int _mi_prim_decommit(void* addr, size_t size, bool* needs_recommit) {
  MI_UNUSED(addr);
  MI_UNUSED(size);
  *needs_recommit = false;
  return 0;
}

int _mi_prim_reset(void* addr, size_t size) {
  MI_UNUSED(addr);
  MI_UNUSED(size);
  return 0;
}

int _mi_prim_reuse(void* addr, size_t size) {
  MI_UNUSED(addr);
  MI_UNUSED(size);
  return 0;
}

int _mi_prim_protect(void* addr, size_t size, bool protect) {
  MI_UNUSED(addr);
  MI_UNUSED(size);
  MI_UNUSED(protect);
  return 0;
}

int _mi_prim_alloc_huge_os_pages(void* hint_addr, size_t size, int numa_node, bool* is_zero,
                                 void** addr) {
  MI_UNUSED(hint_addr);
  MI_UNUSED(size);
  MI_UNUSED(numa_node);
  *is_zero = true;
  *addr = NULL;
  return ENOSYS;
}

size_t _mi_prim_numa_node(void) { return 0; }

size_t _mi_prim_numa_node_count(void) { return 1; }

// No clock: purging, the one use of time that changes behaviour, frees nothing
// in a memory that never shrinks, and is turned off.
mi_msecs_t _mi_prim_clock_now(void) { return 0; }

void _mi_prim_process_info(mi_process_info_t* pinfo) { MI_UNUSED(pinfo); }

void _mi_prim_out_stderr(const char* msg) { MI_UNUSED(msg); }

bool _mi_prim_getenv(const char* name, char* result, size_t result_size) {
  MI_UNUSED(name);
  MI_UNUSED(result);
  MI_UNUSED(result_size);
  return false;
}

bool _mi_prim_random_buf(void* buf, size_t buf_len) {
  MI_UNUSED(buf);
  MI_UNUSED(buf_len);
  return false;
}

// Threads: the caller ends each one with `mi_thread_done`.

void _mi_prim_thread_init_auto_done(void) {}

void _mi_prim_thread_done_auto_done(void) {}

void _mi_prim_thread_associate_default_theap(mi_theap_t* theap) { MI_UNUSED(theap); }

bool _mi_prim_thread_is_in_threadpool(void) { return false; }

void _mi_prim_thread_yield(void) {}

// Process: no loader runs constructors here, so the caller starts mimalloc
// with `bviz_mimalloc_process_init`.

bool _mi_is_redirected(void) { return false; }

bool _mi_allocator_init(const char** message) {
  if (message != NULL) {
    *message = NULL;
  }
  return true;
}

void _mi_allocator_done(void) {}

// mimalloc reserves up to MI_MAX_ARENAS - 4 arenas, all past the sixteenth at
// the cap.
#define WASM32_MEMORY_MAX (4ull << 30)
#define MI_ARENAS_AT_THE_CAP (MI_MAX_ARENAS - 4 - 16)
_Static_assert(MI_ARENAS_AT_THE_CAP * (unsigned long long)MI_ARENA_MAX_RESERVE >= WASM32_MEMORY_MAX,
               "mimalloc's arenas do not cover the memory");

// bviz_alloc checks these against its own as mimalloc starts.
const size_t bviz_mimalloc_max_block = MI_LARGE_MAX_OBJ_SIZE;
const size_t bviz_mimalloc_max_align = MI_MAX_ALIGN_SIZE;
#ifndef MI_ALIGN4W
#error "mimalloc no longer rounds blocks below MI_MAX_ALIGN_SIZE to a power of two"
#endif

uintptr_t bviz_tls_base(void) { return (uintptr_t)__builtin_wasm_tls_base(); }

void bviz_mimalloc_process_init(void) {
  mi_option_set_default(mi_option_purge_delay, -1);
  _mi_auto_process_init();
}

