// SPDX-License-Identifier: GPL-2.0-or-later
/* Adapted from openEuler SysCare upatch-manage 1.2.2-8.oe2403sp4.
 * Copyright (C) 2024 Huawei Technologies Co., Ltd.
 * See COPYING. Gaia adaptation: bounded SEIZE stops, exact thread identities,
 * exact VMA-bounded reads and separate busy/error results. */
#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ptrace.h>
#include <sys/user.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

typedef long (*trace_fn)(unsigned, int, uint64_t, uint64_t);
struct thread { int tid, stopped, signal; };
struct context { int pid, mem; size_t count; struct thread threads[4096]; trace_fn trace; };
struct range { uint64_t start, end; };
static uint64_t now_ms(void) {
    struct timespec t;
    if (clock_gettime(CLOCK_MONOTONIC, &t)) return UINT64_MAX;
    return (uint64_t)t.tv_sec * 1000 + t.tv_nsec / 1000000;
}
void *gaia_upatch_new(int pid, trace_fn trace) {
    struct context *c = calloc(1, sizeof(*c));
    if (c) { c->pid = pid; c->mem = -1; c->trace = trace; }
    return c;
}
static int wait_stop(struct context *c, struct thread *t, uint64_t deadline) {
    while (!t->stopped) {
        int status;
        int r = waitpid(t->tid, &status, __WALL | WNOHANG);
        if (r < 0) { if (errno == EINTR) continue; return -errno; }
        if (r == t->tid) {
            if (!WIFSTOPPED(status)) { t->tid = 0; return -ESRCH; }
            t->stopped = 1;
            if ((status >> 16) != PTRACE_EVENT_STOP || WSTOPSIG(status) != SIGTRAP) {
                t->signal = WSTOPSIG(status);
                return -EINTR;
            }
            return 0;
        }
        if (now_ms() >= deadline) return -ETIMEDOUT;
        usleep(100);
    }
    (void)c;
    return 0;
}
/* Port of upatch_process_attach/process_list_threads: iterate until every
 * enumerated identity is held. Comparing counts alone is insufficient. */
int gaia_upatch_stop(void *opaque, unsigned timeout_ms) {
    struct context *c = opaque;
    char path[80];
    uint64_t deadline = now_ms() + timeout_ms;
    snprintf(path, sizeof(path), "/proc/%d/task", c->pid);
    for (int attempt = 0; attempt < 100; attempt++) {
        DIR *dir = opendir(path);
        if (!dir) return -errno;
        size_t found = 0, added = 0;
        struct dirent *entry;
        int error = 0;
        errno = 0;
        while ((entry = readdir(dir))) {
            char *end;
            long tid = strtol(entry->d_name, &end, 10);
            if (*end || tid <= 0) continue;
            found++;
            size_t i;
            for (i = 0; i < c->count; i++) if (c->threads[i].tid == tid) break;
            if (i < c->count) continue;
            if (c->count == 4096) { error = -E2BIG; break; }
            long ret = c->trace(PTRACE_SEIZE, tid, 0, 0);
            if (ret < 0) { error = (int)ret; break; }
            struct thread *t = &c->threads[c->count++];
            t->tid = tid;
            added++;
            ret = c->trace(PTRACE_INTERRUPT, tid, 0, 0);
            if (ret < 0) { error = (int)ret; break; }
            error = wait_stop(c, t, deadline);
            if (error) break;
            errno = 0;
        }
        if (!error && errno) error = -errno;
        closedir(dir);
        if (error) return error;
        if (!added && found == c->count && found) {
            snprintf(path, sizeof(path), "/proc/%d/mem", c->pid);
            c->mem = open(path, O_RDONLY | O_CLOEXEC);
            return c->mem < 0 ? -errno : 0;
        }
        if (now_ms() >= deadline) return -ETIMEDOUT;
    }
    return -EAGAIN;
}
size_t gaia_upatch_tids(void *opaque, int *out) {
    struct context *c = opaque;
    for (size_t i = 0; i < c->count; i++) out[i] = c->threads[i].tid;
    return c->count;
}
/* Port of stack_check. Inclusive end also protects return addresses immediately
 * after the last call in a function (conservative boundary false positives). */
static int stack_check(const struct range *ranges, size_t count, uint64_t pc) {
    for (size_t i = 0; i < count; i++)
        if (pc >= ranges[i].start && pc <= ranges[i].end) return 1;
    return 0;
}
/* Replace RLIMIT_STACK (the controller's limit!) with the actual stopped VMA.
 * Only private anonymous rw native stacks are accepted. Never accept a prefix
 * read or interpret signed -1 as an allocation/iteration size. */
static int stack_end(struct context *c, uint64_t sp, uint64_t *end) {
    char path[80], *line = NULL;
    size_t cap = 0;
    snprintf(path, sizeof(path), "/proc/%d/maps", c->pid);
    FILE *f = fopen(path, "r");
    if (!f) return -errno;
    int ret = -EFAULT;
    while (getline(&line, &cap, f) >= 0) {
        unsigned long a, b, off, inode;
        char perms[5], dev[32], name[512] = "";
        int fields = sscanf(line, "%lx-%lx %4s %lx %31s %lu %511[^\n]", &a, &b, perms, &off, dev, &inode, name);
        if (fields < 6) { ret = -EINVAL; break; }
        if (sp < a || sp >= b) continue;
        char *n = name; while (*n == ' ') n++;
        if (strcmp(perms, "rw-p") || inode || (*n && strcmp(n, "[stack]")) ||
            b - sp > 64UL * 1024 * 1024 || sp % 8 || (b - sp) % 8) break;
        *end = b;
        ret = 0;
        break;
    }
    if (ferror(f)) ret = -EIO;
    free(line); fclose(f);
    return ret;
}
static int stack_check_each_pid(struct context *c, const struct range *ranges,
    size_t count, int tid, int fault) {
    struct user_regs_struct regs;
    long ret = c->trace(PTRACE_GETREGS, tid, 0, (uintptr_t)&regs);
    if (ret < 0) return (int)ret;
    if (stack_check(ranges, count, regs.rip)) return 1;
    uint64_t end = 0;
    ret = stack_end(c, fault == 3 ? 1 : regs.rsp, &end);
    if (ret) return (int)ret;
    unsigned long stack[512];
    for (uint64_t sp = regs.rsp; sp < end;) {
        size_t size = end - sp < sizeof(stack) ? end - sp : sizeof(stack);
        ssize_t bytes;
        do { bytes = pread(fault == 1 ? -1 : c->mem, stack, fault == 2 ? size - 1 : size, (off_t)sp); } while (bytes < 0 && errno == EINTR);
        if (bytes < 0 || (size_t)bytes != size) return -EIO;
        for (size_t i = 0; i < size / sizeof(*stack); i++) {
            if (stack[i] == 0 || stack[i] == -1UL) continue;
            if (stack_check(ranges, count, stack[i])) return 1;
        }
        sp += size;
    }
    return 0;
}
int gaia_upatch_check(void *opaque, const struct range *ranges, size_t count, int fault) {
    struct context *c = opaque;
    if (!count || !c->count || c->mem < 0) return -EINVAL;
    for (size_t i = 0; i < c->count; i++) {
        if (!c->threads[i].stopped) return -EINVAL;
        int ret = stack_check_each_pid(c, ranges, count, c->threads[i].tid, fault);
        if (ret) return ret; /* errors must never be skipped */
    }
    return 0;
}
/* Never kill the target on cleanup errors. Retain context for explicit retry. */
int gaia_upatch_release(void *opaque) {
    struct context *c = opaque;
    int error = 0;
    for (size_t i = c->count; i > 0; i--) {
        struct thread *t = &c->threads[i-1];
        if (!t->tid) continue;
        if (!t->stopped) {
            c->trace(PTRACE_INTERRUPT, t->tid, 0, 0);
            int r = wait_stop(c, t, now_ms() + 1000);
            if (r && !t->stopped && t->tid) { error = r; continue; }
        }
        if (!t->tid) continue;
        long ret = c->trace(PTRACE_DETACH, t->tid, 0, t->signal);
        if (ret < 0) { error = (int)ret; continue; }
        t->tid = 0;
    }
    if (!error) { if (c->mem >= 0) close(c->mem); free(c); }
    return error;
}
