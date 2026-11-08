#define _GNU_SOURCE
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <stdatomic.h>
#include <sys/prctl.h>
// No SafePatch ABI, gate, active counter, or wrapper.
__attribute__((naked,noinline,aligned(16))) int target_value(void) {
    __asm__("mov $10,%eax; ret; nop; nop");
}
__attribute__((naked,noinline,aligned(16))) void conflict(void) {
    __asm__("1: jmp 1b; nop; nop; nop; nop; nop; nop");
}
static _Atomic int ready;
static void *worker(void *arg) {
    atomic_fetch_add(&ready,1);
    if (arg) conflict();
    for (;;) { int n=target_value(); if(n!=10 && n!=99) abort(); usleep(100); }
}
static void *short_lived(void *arg) { return arg; }
int main(int argc,char **argv) {
    int churn=argc>1 && strcmp(argv[1],"churn")==0;
    int busy=argc>1 && !churn;
    prctl(PR_SET_PTRACER,getppid(),0,0,0);
    pthread_t t[8];
    for(int i=0;i<8;i++) if(pthread_create(&t[i],0,worker,busy?(void*)1:0)) return 1;
    while(atomic_load(&ready)!=8) sched_yield();
    printf("READY %d %p\n",getpid(),busy?(void*)conflict:(void*)target_value); fflush(stdout);
    for(;;) {
        if(churn) { pthread_t transient; if(pthread_create(&transient,0,short_lived,0)) return 2; pthread_join(transient,0); }
        else pause();
    }
}
