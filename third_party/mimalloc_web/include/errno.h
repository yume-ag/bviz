#ifndef _ERRNO_H
#define _ERRNO_H

// Written, never read: nothing in the browser inspects it.
extern _Thread_local int errno;

#define EFAULT 14
#define EAGAIN 11
#define ENOMEM 12
#define EINVAL 22
#define ENOSYS 38
#define EOVERFLOW 75

#endif
