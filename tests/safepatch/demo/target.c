#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <semaphore.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <time.h>
#include <unistd.h>
#include <sys/prctl.h>

#include "cooperative.h"

#ifndef DEMO_TASK_MS
#define DEMO_TASK_MS 800
#endif

static sem_t jobs[8];
static const char *output_dir;

static uint64_t now_ns(void)
{
    struct timespec time;
    clock_gettime(CLOCK_MONOTONIC, &time);
    return time.tv_sec * 1000000000ULL + time.tv_nsec;
}

/* 用本地替身模拟排版服务的正常处理耗时，不参与补丁安全判断。 */
__attribute__((noinline))
static int layout_document(int pages)
{
    printf("LAYOUT\t【排版中】已收到文档，预计处理 %d 毫秒。\n", DEMO_TASK_MS);
    struct timespec delay = {DEMO_TASK_MS / 1000, (DEMO_TASK_MS % 1000) * 1000000L};
    while (nanosleep(&delay, &delay) != 0)
    {
        if (errno != EINTR)
        {
            perror("排版等待失败");
            exit(1);
        }
        /* 被信号打断时继续等待剩余时间。 */
    }
    return pages * 40;
}

/* Gaia 修改此普通 C 函数：维护版本直接返回 0，表示不再导出。 */
__attribute__((noinline))
int render_document(int pages)
{
    int lines = layout_document(pages);
    return lines > 0;
}

static void *worker(void *arg)
{
    long id = (long)arg;
    char path[512];
    snprintf(path, sizeof(path), "%s/document-%ld.txt", output_dir, id);

    for (;;)
    {
        while (sem_wait(&jobs[id]) != 0)
        {
            if (errno != EINTR)
            {
                perror("读取作业队列失败");
                exit(1);
            }
        }

        enter_region();
        int exported = render_document(20);
        uint64_t begin = now_ns();
        if (exported)
        {
            FILE *file = fopen(path, "w");
            if (file == NULL)
            {
                perror("无法创建输出文件");
                exit(1);
            }
            if (fprintf(file, "文档 %ld：20 页排版完成。\n", id) < 0 || fclose(file) != 0)
            {
                perror("无法完成输出文件");
                exit(1);
            }
        }
        uint64_t end = now_ns();
        printf("DONE %ld %d %lu %lu\t作业 %ld：%s\n",
               id, exported, begin, end, id,
               exported ? "已生成文档文件" : "维护中，未生成文件");
        leave_region();
    }
    return NULL;
}

int main(int argc, char **argv)
{
    if (argc != 2)
    {
        fprintf(stderr, "用法：文档服务 输出目录\n");
        return 1;
    }
    output_dir = argv[1];
    prctl(PR_SET_PTRACER, getppid(), 0, 0, 0);
    setvbuf(stdout, NULL, _IOLBF, 0);

    pthread_t threads[8];
    for (long id = 0; id < 8; id++)
    {
        if (sem_init(&jobs[id], 0, 0) != 0 ||
            pthread_create(&threads[id], NULL, worker, (void *)id) != 0)
        {
            fprintf(stderr, "无法启动工作线程\n");
            return 1;
        }
    }
    printf("READY %d %p %p\n", getpid(), (void *)render_document, DEMO_STATE);

    /* 输入作业编号即可提交文档；主线程仅负责分发，不代替工作线程排版。 */
    int job;
    while (scanf("%d", &job) == 1)
    {
        if (job >= 0 && job < 8)
        {
            sem_post(&jobs[job]);
        }
    }
    return 0;
}
