#define _GNU_SOURCE
#include <pthread.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <time.h>
#include <unistd.h>

/* Test fixture only: same opt-in ABI, stable thread set, sampled real returns. */
#ifndef NO_ABI
_Alignas(8) _Atomic uint64_t gaia_safepatch_state[3] = {0x4741494153503031ULL, 0, 0};
#endif
__attribute__((naked,noinline,noclone,used,aligned(16))) int target_value(void) {
    __asm__("mov $10,%eax\n\tret\n\t.rept 10\n\tnop\n\t.endr");
}
static int (*volatile invoke)(void) = target_value;
static unsigned long long stamp(void) {
    struct timespec t; clock_gettime(CLOCK_MONOTONIC, &t);
    return (unsigned long long)t.tv_sec*1000000000ULL+t.tv_nsec;
}
static void *worker(void *arg) {
    for (;;) {
#ifndef NO_ABI
        while (atomic_load(&gaia_safepatch_state[1])) sched_yield();
        atomic_fetch_add(&gaia_safepatch_state[2], 1);
        if (atomic_load(&gaia_safepatch_state[1])) {
            atomic_fetch_sub(&gaia_safepatch_state[2], 1);
            continue;
        }
#endif
        unsigned long long begin = stamp();
        int result = invoke();
        unsigned long long end = stamp();
#ifndef NO_ABI
        atomic_fetch_sub(&gaia_safepatch_state[2], 1);
#endif
        printf("VALUE %ld %d %llu %llu\n", (long)arg, result, begin, end);
        usleep(1000);
    }
    return NULL;
}
int main(void) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    pthread_t threads[4];
    for (long i=0; i<4; i++) if (pthread_create(&threads[i], NULL, worker, (void*)i)) return 2;
    printf("READY %d %p\n", getpid(), (void*)target_value);
    for (;;) pause();
}
