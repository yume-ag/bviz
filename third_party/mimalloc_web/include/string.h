#ifndef _STRING_H
#define _STRING_H

#include <stddef.h>

// The Rust standard library's compiler_builtins defines these on WebAssembly.
void* memcpy(void* restrict dest, const void* restrict src, size_t n);
void* memmove(void* dest, const void* src, size_t n);
void* memset(void* dest, int c, size_t n);
int memcmp(const void* a, const void* b, size_t n);

#endif
