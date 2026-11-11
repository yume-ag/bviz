#ifndef _STDLIB_H
#define _STDLIB_H

#include <stddef.h>

static inline _Noreturn void abort(void) { __builtin_trap(); }

// Parses nothing, as no environment holds a number to parse.
static inline long strtol(const char* text, char** end, int base) {
  (void)base;
  if (end != NULL) {
    *end = (char*)text;
  }
  return 0;
}

// The browser has no file system.
static inline char* realpath(const char* path, char* resolved) {
  (void)path;
  (void)resolved;
  return NULL;
}

#endif
