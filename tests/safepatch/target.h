#define _GNU_SOURCE
#include <pthread.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <time.h>
#include <unistd.h>
#include <sys/prctl.h>
struct { _Atomic uint64_t magic, gate, active; } gaia_safepatch_state = {0x4741494153503031, 0, 0};
_Atomic uint64_t gaia_risk_release;
static pthread_barrier_t barrier;
_Atomic uint64_t calls;
static uint64_t now(void) { struct timespec t; clock_gettime(CLOCK_MONOTONIC, &t); return t.tv_sec*1000000000ULL+t.tv_nsec; }
__attribute__((noinline,noclone,aligned(16))) int target_value(void) {
#if LONG_CALL
    struct timespec delay = {0, 200000000}; nanosleep(&delay, 0);
#endif
    return 10;
}
static void *worker(void *arg) {
    long id = (long)arg;
    pthread_barrier_wait(&barrier);
    if (getenv("GAIA_RISK") && id != 0) while (!atomic_load(&gaia_risk_release)) sched_yield();
    for (;;) {
#ifndef GAIA_BASELINE
        while (atomic_load(&gaia_safepatch_state.gate)) sched_yield();
        atomic_fetch_add(&gaia_safepatch_state.active, 1);
        if (atomic_load(&gaia_safepatch_state.gate)) { atomic_fetch_sub(&gaia_safepatch_state.active, 1); continue; }
#endif
        uint64_t start = now();
        int value = target_value();
        uint64_t end = now();
        if (value != 10 && value != 99) { fprintf(stderr,"INVALID %d\n",value); abort(); }
#if LONG_CALL
        printf("CALL %ld %lu %lu %d\n",id,start,end,value);
#endif
        atomic_fetch_add(&calls, 1);
#ifndef GAIA_BASELINE
        atomic_fetch_sub(&gaia_safepatch_state.active, 1);
#endif
    }
    return 0;
}
int main(void) {
    // Test harness is the sole authorized external tracer, even with Yama enabled.
    prctl(PR_SET_PTRACER, getppid(), 0, 0, 0);
    setvbuf(stdout, NULL, _IOLBF, 0);
    pthread_t threads[8];
    pthread_barrier_init(&barrier, NULL, 9);
    for(long i=0;i<8;i++) if(pthread_create(&threads[i],0,worker,(void*)i)) return 1;
    printf("READY %d %p %p %p %p\n",getpid(),(void*)target_value,(void*)&gaia_safepatch_state,(void*)&gaia_risk_release,(void*)&calls);
    pthread_barrier_wait(&barrier);
    for (;;) { sleep(1); printf("COUNT %lu %lu\n",now(),atomic_load(&calls)); }
}
