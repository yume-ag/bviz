#ifndef _PTHREAD_H
#define _PTHREAD_H

#include <stdint.h>

// bviz_alloc's lock, which spins briefly and then parks a worker, but only
// spins on the browser's main thread, where `memory.atomic.wait` traps.
typedef struct {
  uint32_t state;
} pthread_mutex_t;

#define PTHREAD_MUTEX_INITIALIZER {0}

void bviz_lock_acquire(pthread_mutex_t* mutex);
_Bool bviz_lock_try_acquire(pthread_mutex_t* mutex);
void bviz_lock_release(pthread_mutex_t* mutex);

static inline int pthread_mutex_trylock(pthread_mutex_t* mutex) {
  return bviz_lock_try_acquire(mutex) ? 0 : 1;
}

static inline int pthread_mutex_lock(pthread_mutex_t* mutex) {
  bviz_lock_acquire(mutex);
  return 0;
}

static inline int pthread_mutex_unlock(pthread_mutex_t* mutex) {
  bviz_lock_release(mutex);
  return 0;
}

static inline int pthread_mutex_destroy(pthread_mutex_t* mutex) {
  (void)mutex;
  return 0;
}

#endif
