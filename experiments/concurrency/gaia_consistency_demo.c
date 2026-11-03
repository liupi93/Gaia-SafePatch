#define _POSIX_C_SOURCE 200809L
#include <errno.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

#define READY_FILE "/tmp/gaia-consistency.ready"
#define CONTINUE_FILE "/tmp/gaia-consistency.continue"
static pthread_barrier_t first_call_barrier;
static atomic_int ready_announced = 0;
static int thread_count = 8;

/* Gaia overwrites the first eight bytes of this visible, unoptimised symbol. */
__attribute__((noinline, visibility("default")))
int pricing_rule(int input) {
    volatile int stable_input = input;
    return stable_input + 100;
}

static void wait_for_file(const char *path) {
    struct timespec delay = {.tv_sec = 0, .tv_nsec = 10 * 1000 * 1000};
    while (access(path, F_OK) != 0) nanosleep(&delay, NULL);
}

static void *run_request(void *opaque) {
    intptr_t request_id = (intptr_t)opaque;
    int before = pricing_rule((int)request_id);
    int rc = pthread_barrier_wait(&first_call_barrier);
    if (rc != 0 && rc != PTHREAD_BARRIER_SERIAL_THREAD) {
        fprintf(stderr, "barrier failed: %s\n", strerror(rc));
        return NULL;
    }
    if (atomic_exchange(&ready_announced, 1) == 0) {
        FILE *ready = fopen(READY_FILE, "w");
        if (!ready) {
            fprintf(stderr, "cannot create %s: %s\n", READY_FILE, strerror(errno));
            return NULL;
        }
        fprintf(ready, "%ld\n", (long)getpid());
        fclose(ready);
        printf("READY pid=%ld active_business_requests=%d\n", (long)getpid(), thread_count);
    }
    wait_for_file(CONTINUE_FILE);
    int after = pricing_rule((int)request_id);
    const char *kind =
        (before == (int)request_id + 100 && after == 200) ? "MIXED" :
        (before == (int)request_id + 100 && after == (int)request_id + 100) ? "OLD_ONLY" :
        (before == 200 && after == 200) ? "NEW_ONLY" : "UNEXPECTED";
    printf("RESULT request=%ld tid=%lu before=%d after=%d classification=%s\n",
           (long)request_id, (unsigned long)pthread_self(), before, after, kind);
    return NULL;
}

int main(int argc, char **argv) {
    if (argc == 2) thread_count = atoi(argv[1]);
    if (thread_count < 2 || thread_count > 256) {
        fprintf(stderr, "thread count must be in [2, 256]\n");
        return 2;
    }
    unlink(READY_FILE); unlink(CONTINUE_FILE);
    setvbuf(stdout, NULL, _IOLBF, 0);
    if (pthread_barrier_init(&first_call_barrier, NULL, (unsigned)thread_count) != 0) {
        perror("pthread_barrier_init"); return 1;
    }
    pthread_t *threads = calloc((size_t)thread_count, sizeof(*threads));
    if (!threads) { perror("calloc"); return 1; }
    printf("START pid=%ld threads=%d symbol=pricing_rule old_semantics=input+100\n",
           (long)getpid(), thread_count);
    for (intptr_t i = 0; i < thread_count; ++i) {
        int rc = pthread_create(&threads[i], NULL, run_request, (void *)i);
        if (rc != 0) { fprintf(stderr, "pthread_create: %s\n", strerror(rc)); return 1; }
    }
    for (int i = 0; i < thread_count; ++i) pthread_join(threads[i], NULL);
    pthread_barrier_destroy(&first_call_barrier); free(threads);
    unlink(READY_FILE); unlink(CONTINUE_FILE); puts("DONE");
    return 0;
}
