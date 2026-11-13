#ifndef _STDIO_H
#define _STDIO_H

#include <stddef.h>

// The browser has no standard streams.
typedef struct FILE FILE;

#define stdout ((FILE*)0)
#define stderr ((FILE*)0)

#endif
