#define _GNU_SOURCE
#include <pthread.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
#include <sys/prctl.h>
#include <sys/resource.h>

// 协作模式
#ifdef COOPERATIVE
// 协作接口结构体
struct { _Atomic uint64_t magic, gate, active; } gaia_safepatch_state={0x4741494153503031,0,0};

// >> 接口宏定义
#define STATE ((void*)&gaia_safepatch_state)
#else
#define STATE ((void*)0)
#endif
// << 接口宏定义

// 状态量
_Atomic uint64_t release_orders, old_started, completed, late_old, new_seen;
static int entry_scene;
// Deliberate adversarial entry: after xor, RIP stays exactly at entry+2.
// Before patch it is a valid infinite loop, with no memory access or crash.
__attribute__((naked,noinline,aligned(16))) int entry_policy(void) {
    __asm__("xor %eax,%eax; 1: jmp 1b; nop; nop; nop; nop");
}
// Eight entry NOPs ensure the long-call scene does not mix in instruction conflicts.
__attribute__((noinline,noclone,patchable_function_entry(8,0),aligned(16)))
int discount_policy(void) {
    atomic_store(&old_started,1);
    usleep(1200000);
    return 10;
}
static void enter(void) {
#ifdef COOPERATIVE
    for (;;) {
        while(atomic_load(&gaia_safepatch_state.gate)) sched_yield();
        atomic_fetch_add(&gaia_safepatch_state.active,1);
        if (!atomic_load(&gaia_safepatch_state.gate)) break;
        atomic_fetch_sub(&gaia_safepatch_state.active,1);
    }
#endif
}
static void *worker(void *arg) {
    long id=(long)arg;
    if(entry_scene) {
        if(id==0) { atomic_store(&old_started,1); entry_policy(); }
    } else {
        if(id) while(!atomic_load(&release_orders)) usleep(1000);
        enter();
        int discount=discount_policy();
        // All commit logic stays inside the cooperative region.
        if(discount==99) atomic_store(&new_seen,1);
        if(discount==10 && atomic_load(&new_seen)) atomic_fetch_add(&late_old,1);
        if(discount!=10 && discount!=99) abort();
        printf("订单 %ld｜折扣值 %d｜%s\n",id,discount,
            discount==10 && atomic_load(&new_seen)?"警告：旧结果迟到了！":"提交完成");
#ifdef COOPERATIVE
        atomic_fetch_sub(&gaia_safepatch_state.active,1);
#endif
        atomic_fetch_add(&completed,1);
    }
    for(;;) pause(); // Keep a fixed thread set even after each order finishes.
    return 0;
}
int main(int argc,char **argv) {
    struct rlimit core={0,0}; setrlimit(RLIMIT_CORE,&core);
    prctl(PR_SET_PTRACER,getppid(),0,0,0);
    setvbuf(stdout,0,_IOLBF,0);
    entry_scene=argc>1;
    pthread_t threads[8];
    for(long i=0;i<8;i++) if(pthread_create(&threads[i],0,worker,(void*)i)) return 1;
    printf("READY %d %p %p %p %p %p %p\n",getpid(),
        entry_scene?(void*)entry_policy:(void*)discount_policy,
        STATE,(void*)&release_orders,(void*)&old_started,
        (void*)&completed,(void*)&late_old);
    for(;;) pause();
}
