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
#include "../demo/cooperative.h"
#ifndef LAYOUT_MS
#define LAYOUT_MS 1000
#endif
#ifndef PRINT_MS
#define PRINT_MS 2500
#endif
struct job { int id; uint64_t begin; char path[512]; };
struct queue { pthread_mutex_t lock; sem_t ready; struct job *items[128]; unsigned head,tail; };
static struct queue incoming={.lock=PTHREAD_MUTEX_INITIALIZER}, printing={.lock=PTHREAD_MUTEX_INITIALIZER};
static const char *directory;
static _Thread_local int current_job;
static uint64_t now(void) {struct timespec t; clock_gettime(CLOCK_MONOTONIC,&t);return t.tv_sec*1000000000ULL+t.tv_nsec;}
static void delay(unsigned ms) {
    struct timespec t={ms/1000,(ms%1000)*1000000L};
    while(nanosleep(&t,&t)) if(errno!=EINTR) abort();
}
static void push(struct queue *q,struct job *j) {
    pthread_mutex_lock(&q->lock);
    if(q->tail-q->head==128) abort();
    q->items[q->tail++%128]=j;
    pthread_mutex_unlock(&q->lock); sem_post(&q->ready);
}
static struct job *pop(struct queue *q) {
    while(sem_wait(&q->ready)) if(errno!=EINTR) abort();
    pthread_mutex_lock(&q->lock);struct job *j=q->items[q->head++%128];pthread_mutex_unlock(&q->lock);return j;
}
/* Rendering authorizes export; the printer queue owns subsequent output work. */
__attribute__((noinline)) int render_document(int pages) {
    printf("BEGIN %d %lu\n",current_job,now());
    delay(LAYOUT_MS);
    return pages>0;
}
/* Identical output operation in both business models. */
static void print_document(struct job *j) {
        char temporary[600];snprintf(temporary,sizeof(temporary),"%s.part",j->path);
        FILE *f=fopen(temporary,"w");if(!f) abort();
        printf("PRINT_BEGIN %d %lu\n",j->id,now());
        /* A paced printer substitute writes real data in chunks. This same
         * device latency applies to all modes, independently of patch status. */
        for(int page=0;page<10;page++) {
            if(fprintf(f,"Document %d page %d\n",j->id,page)<0 || fflush(f)) abort();
            delay(PRINT_MS/10);
        }
        if(fclose(f)) abort();
        uint64_t closed=now();
        if(rename(temporary,j->path)) abort();
        uint64_t committed=now();
        printf("COMMIT %d %lu %lu\n",j->id,closed,committed);
}
#ifdef ALIGNED_BOUNDARY
/* One finite call owns the whole job, including final document publication. */
__attribute__((noinline)) int export_document(struct job *j) {
    printf("BEGIN %d %lu\n",j->id,now());
    delay(LAYOUT_MS);
    print_document(j);
    return 1;
}
#define PATCH_TARGET export_document
#else
#define PATCH_TARGET render_document
#endif
static void *renderer(void *unused) {
    (void)unused;
    for(;;) {
        struct job *j=pop(&incoming);current_job=j->id;
        enter_region();
#ifdef ALIGNED_BOUNDARY
        int accepted=export_document(j);
        printf("RENDER_END %d %lu\n",j->id,now());
        if(!accepted) printf("REJECT %d %lu\n",j->id,now());
        leave_region();free(j);
#else
        int accepted=render_document(20);
        printf("RENDER_END %d %lu\n",j->id,now());
        if(accepted) push(&printing,j); /* Count obligation moves to consumer. */
        else {printf("REJECT %d %lu\n",j->id,now());leave_region();free(j);}
#endif
    }
    return NULL;
}
static void *printer(void *unused) {
    (void)unused;
    for(;;) {
        struct job *j=pop(&printing);
        print_document(j);
        leave_region();free(j);
    }
    return NULL;
}
int main(int argc,char **argv) {
    if(argc!=2) return 2;directory=argv[1];
    prctl(PR_SET_PTRACER,getppid(),0,0,0);setvbuf(stdout,NULL,_IOLBF,0);
    sem_init(&incoming.ready,0,0);sem_init(&printing.ready,0,0);
    pthread_t threads[8];
    for(int i=0;i<4;i++) if(pthread_create(&threads[i],0,renderer,0)||pthread_create(&threads[4+i],0,printer,0)) return 3;
    printf("READY %d %p %p\n",getpid(),(void*)PATCH_TARGET,DEMO_STATE);
    int id;
    while(scanf("%d",&id)==1) {
        struct job *j=calloc(1,sizeof(*j));if(!j) abort();j->id=id;
        snprintf(j->path,sizeof(j->path),"%s/document-%d.txt",directory,id);push(&incoming,j);
    }
    return 0;
}
