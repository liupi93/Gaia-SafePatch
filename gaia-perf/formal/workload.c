/* Fixed-concurrency Linux x86-64 workload for the real Gaia test. */
#define _POSIX_C_SOURCE 200809L
#include <inttypes.h>
#include <limits.h>
#include <pthread.h>
#include <sched.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

/* Same opt-in ABI as tests/safepatch/target.h in the optimized branch. */
_Alignas(8) _Atomic uint64_t gaia_safepatch_state[3] = {UINT64_C(0x4741494153503031), 0, 0};
enum { READY, WARMUP, SCHEDULED, STOPPED };
static _Atomic int phase;
static _Atomic uint64_t first_new_return;
static uint64_t window_start, window_end, seed;
static int cooperative;
static const char *run_id;

struct request {
    uint64_t arrival, begin, end;
    int measure, ok;
};
struct worker {
    pthread_t thread;
    struct request *rows;
    size_t count, capacity;
    unsigned id;
    uint32_t state;
};

static uint64_t now_ns(void) {
    struct timespec t;
    if (clock_gettime(CLOCK_MONOTONIC, &t)) abort();
    return (uint64_t)t.tv_sec * UINT64_C(1000000000) + (uint64_t)t.tv_nsec;
}

/* >= 8 writable bytes at entry, with no inlining, cloning or CET prefix. */
__attribute__((naked, noinline, noclone, used, aligned(16)))
int target_value(void) {
    __asm__("mov $10,%eax\n\tret\n\t.rept 10\n\tnop\n\t.endr");
}
static int (*volatile call_target)(void) = target_value;

static int admit(void) {
    if (!cooperative) return 1;
    while (atomic_load(&phase) != STOPPED) {
        if (atomic_load(&gaia_safepatch_state[1])) {
            sched_yield();
            continue;
        }
        atomic_fetch_add(&gaia_safepatch_state[2], 1);
        if (!atomic_load(&gaia_safepatch_state[1])) return 1;
        atomic_fetch_sub(&gaia_safepatch_state[2], 1);
    }
    return 0;
}

static void append(struct worker *w, struct request row) {
    if (w->count == w->capacity) {
        size_t capacity = w->capacity ? 2 * w->capacity : 4096;
        if (capacity > 262144) abort();
        struct request *next = realloc(w->rows, capacity * sizeof(*next));
        if (!next) abort();
        w->rows = next;
        w->capacity = capacity;
    }
    w->rows[w->count++] = row;
}

static void *work(void *argument) {
    struct worker *w = argument;
    uint32_t value = (uint32_t)(seed + w->id + 1);
    const struct timespec idle = {.tv_nsec = 200000};
    for (;;) {
        uint64_t arrival = now_ns();
        int current = atomic_load(&phase);
        if (current == STOPPED) break;
        if (current == READY || (current == SCHEDULED && arrival >= window_end)) {
            nanosleep(&idle, NULL);
            continue;
        }
        struct request row = {.arrival = arrival, .measure = current == SCHEDULED && arrival >= window_start};
        if (admit()) {
            row.begin = now_ns();
            /* Identical service work and per-worker seed in both builds and modes. */
            for (unsigned i = 0; i < 1500000; ++i) {
                value ^= value << 13;
                value ^= value >> 17;
                value ^= value << 5;
            }
            int result = call_target();
            row.ok = result == 10 || result == 99;
            row.end = now_ns();
            if (result == 99 && !atomic_load(&first_new_return)) {
                uint64_t empty = 0;
                atomic_compare_exchange_strong(&first_new_return, &empty, row.end);
            }
            if (cooperative) atomic_fetch_sub(&gaia_safepatch_state[2], 1);
        }
        append(w, row);
    }
    w->state = value;
    return NULL;
}

static const char *require_env(const char *key) {
    const char *value = getenv(key);
    if (!value || !*value) {
        fprintf(stderr, "Missing %s\n", key);
        exit(2);
    }
    return value;
}

static void status(const char *name, size_t generated) {
    const char *path = require_env("GAIA_PERF_STATUS_FILE");
    char tmp[PATH_MAX];
    if (snprintf(tmp, sizeof(tmp), "%s.tmp", path) >= (int)sizeof(tmp)) abort();
    FILE *out = fopen(tmp, "w");
    if (!out) abort();
    fprintf(out, "{\"run_id\":\"%s\",\"pid\":%ld,\"clock\":\"CLOCK_MONOTONIC\","
        "\"process_model\":\"single-process-multithread\",\"phase\":\"%s\","
        "\"ack_ns\":%" PRIu64 ",\"start_ns\":%" PRIu64 ",\"end_ns\":%" PRIu64 ","
        "\"cooperative_abi\":%s,\"generated_count\":%zu}\n", run_id, (long)getpid(), name,
        now_ns(), window_start, window_end, cooperative ? "true" : "false", generated);
    if (fclose(out) || rename(tmp, path)) abort();
}

static size_t export_requests(struct worker *workers, unsigned count) {
    FILE *out = fopen(require_env("GAIA_PERF_REQUESTS_FILE"), "w");
    if (!out) abort();
    setvbuf(out, NULL, _IOFBF, 1024 * 1024);
    fputs("run_id,request_id,worker_id,phase,arrival_ns,start_ns,end_ns,status\n", out);
    size_t generated = 0;
    for (unsigned i = 0; i < count; ++i) {
        struct worker *w = &workers[i];
        for (size_t j = 0; j < w->count; ++j) {
            struct request *r = &w->rows[j];
            fprintf(out, "%s,%zu,%u,%s,%" PRIu64 ",", run_id, j, i, r->measure ? "measure" : "warmup", r->arrival);
            if (r->end) fprintf(out, "%" PRIu64 ",%" PRIu64 ",%s\n", r->begin, r->end, r->ok ? "ok" : "error");
            else fputs(",,cancelled\n", out);
            generated += r->measure;
        }
        free(w->rows);
    }
    if (fclose(out)) abort();
    uint64_t observed = atomic_load(&first_new_return);
    if (observed) {
        out = fopen(require_env("GAIA_PERF_PATCH_PROOF_FILE"), "w");
        if (!out) abort();
        fprintf(out, "{\"run_id\":\"%s\",\"pid\":%ld,\"attempt_id\":\"patch-1\","
            "\"observed_ns\":%" PRIu64 ",\"new_implementation\":true,"
            "\"evidence\":\"target_value returned 99\"}\n", run_id, (long)getpid(), observed);
        if (fclose(out)) abort();
    }
    return generated;
}

int main(void) {
    run_id = require_env("GAIA_PERF_RUN_ID");
    seed = strtoull(require_env("GAIA_PERF_SEED"), NULL, 10);
    unsigned count = (unsigned)strtoul(require_env("GAIA_PERF_THREADS"), NULL, 10);
    cooperative = strcmp(require_env("GAIA_WORKLOAD_COOPERATIVE"), "1") == 0;
    if (!count || count > 64) return 2;
    struct worker *workers = calloc(count, sizeof(*workers));
    if (!workers) return 2;
    for (unsigned i = 0; i < count; ++i) {
        workers[i].id = i;
        if (pthread_create(&workers[i].thread, NULL, work, &workers[i])) return 2;
    }
    status("ready", 0);
    char line[256], id[128];
    size_t generated = 0;
    while (fgets(line, sizeof(line), stdin)) {
        if (!strcmp(line, "WARMUP\n") && atomic_load(&phase) == READY) {
            atomic_store(&phase, WARMUP);
            status("warmup", 0);
        } else if (atomic_load(&phase) == WARMUP &&
            sscanf(line, "MEASURE %127s %" SCNu64 " %" SCNu64, id, &window_start, &window_end) == 3 &&
            !strcmp(id, run_id) && window_start < window_end) {
            atomic_store(&phase, SCHEDULED);
            status("measure", 0);
        } else if (!strcmp(line, "STOP\n")) {
            if (atomic_exchange(&phase, STOPPED) != STOPPED) {
                for (unsigned i = 0; i < count; ++i) pthread_join(workers[i].thread, NULL);
                generated = export_requests(workers, count);
                /* Retain the deterministic calculation through an observable final sink. */
                uint32_t checksum = 0;
                for (unsigned i = 0; i < count; ++i) checksum ^= workers[i].state;
                fprintf(stderr, "workload checksum=%" PRIu32 "\n", checksum);
            }
            status("stopped", generated);
        }
    }
    return 0;
}
