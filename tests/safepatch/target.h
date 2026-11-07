#define _GNU_SOURCE // 启用 GNU 扩展特性，以便使用部分 Linux/POSIX 接口
#include <pthread.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <time.h>
#include <unistd.h>
#include <sys/prctl.h>

/// @brief 用于向 Gaia-SafePatch 提供程序状态信息的全局变量。
/// _Atomic 是 C11 引入的原子类型限定符，支持线程间的原子读写和原子运算。
/// @note 目标程序与 Gaia 必须约定这三个字段的内存布局、大小和含义。
/// @note 当前协议使用默认的顺序一致性原子操作（memory_order_seq_cst）。
struct 
{
    _Atomic uint64_t magic, // 魔数，用于识别协议，固定为 0x4741494153503031
    gate, // 是否关闭门：0 正常，1 禁止新的调用进入受保护区域
    active; // 已登记但尚未完成的受保护调用数量，不一定等于正在执行目标函数的线程数
} 
gaia_safepatch_state = {0x4741494153503031, 0, 0};

/// @brief 风险测试的释放标志。
/// @note 在 GAIA_RISK 模式下，除 0 号线程外，其余工作线程等待该变量变为非零值。
/// @note 测试驱动可以通过修改此变量，控制其他线程进入目标函数的时机。
_Atomic uint64_t gaia_risk_release;

/// @brief POSIX 线程同步屏障，用于让主线程和工作线程等待彼此准备完成。
static pthread_barrier_t barrier;

_Atomic uint64_t calls; // 已完成的目标函数调用总次数，多个线程共享，采用原子累加

/// @brief 获取当前单调时钟时间
/// @note CLOCK_MONOTONIC 不受系统墙上时钟调整的直接影响，适合计算执行时间间隔。
/// @note 返回的是纳秒时间戳，而非一次操作的耗时；两个时间戳相减才能得到耗时。
/// @return 当前单调时钟对应的纳秒数（uint64_t）
static uint64_t now(void)
{
    struct timespec t;
    // 获取时间，tv_sec 为秒，tv_nsec 为秒内的纳秒部分
    clock_gettime(CLOCK_MONOTONIC, &t);

    // 将秒换算为纳秒后加上 tv_nsec
    // ULL 表示无符号 long long 整数字面量
    return t.tv_sec*1000000000ULL+t.tv_nsec;
}

/// @brief 热补丁实验的目标函数，Gaia 将修改它在内存中的机器码
/// @note 默认返回 10；安装测试补丁后，后续调用应返回 99。
/// @note 使用 LONG_CALL 编译时，原函数内部会额外等待约 200 ms，用于模拟长时间运行的调用。
/// @param 无参数
/// @return 原始版本返回 10，补丁版本预期返回 99
__attribute__((noinline,noclone,aligned(16))) // 禁止内联、禁止编译器克隆该函数，并要求函数入口按 16 字节对齐
int target_value(void)
{
#if LONG_CALL // 如果启用 LONG_CALL，则模拟长时间运行的函数
    // timespec 的两个字段分别是秒和纳秒
    struct timespec delay = {0, 200000000};

    // 请求睡眠约 200 ms；可能被信号提前中断
    // 睡眠期间线程可能处于阻塞状态，但目标函数调用尚未结束
    nanosleep(&delay, 0);
#endif
    return 10; // 原始返回值是 10
}

/// @brief 每个工作线程执行的入口函数
/// @param arg 工作线程编号，创建线程时通过 pthread_create 传入
/// @return 正常情况下返回 NULL，但当前实现持续循环，通常不会执行到 return
/// @note 8 个工作线程共享同一个 target_value() 的机器码及全局变量。
static void *worker(void *arg)
{
    long id = (long)arg; // 将传入的参数还原为工作线程编号（0～7）

    // 等待全部 8 个工作线程和主线程到达同步屏障
    // 避免部分线程尚未准备好就开始执行测试
    pthread_barrier_wait(&barrier);

    // GAIA_RISK 是环境变量，用来启用可控的原版风险复现实验
    // 0 号线程不受该等待条件限制，可以率先进入旧版目标函数
    if (getenv("GAIA_RISK") && id != 0)
    {
        // 其余 7 个线程等待测试驱动释放
        // atomic_load 保证对释放标志进行原子读取
        while (!atomic_load(&gaia_risk_release)) 
        {
            // 主动让出当前 CPU 的执行机会，但不保证线程进入睡眠或停止状态
            sched_yield();
        }
    }

    // 无限循环，持续调用目标函数以产生并发压力
    // for (;;) 等价于 while (1)，可通过 break 或 return 退出
    for (;;)
    {
#ifndef GAIA_BASELINE // 非基线模式：启用协作式门禁和活动调用计数
        // 第一次检查门禁：如果 gate = 1，说明 Gaia 正在阻止新的调用进入
        // 线程在此等待，直到 gate 重新变为 0
        while (atomic_load(&gaia_safepatch_state.gate))
        {
            sched_yield();
        }

        // 此时第一次检查发现门是开的，但尚未真正进入目标函数
        // 原子增加 active，登记一个尚未完成的受保护调用
        atomic_fetch_add(&gaia_safepatch_state.active, 1);

        // 第二次检查门禁，防止第一次检查与 active 登记之间发生竞争
        // 例如：第一次检查 gate=0，线程被抢占，Gaia 此时关闭了门
        if (atomic_load(&gaia_safepatch_state.gate))
        {
            // 发现登记期间门已关闭，撤销活动调用登记
            atomic_fetch_sub(&gaia_safepatch_state.active, 1);

            // 注意：此时还没有执行 target_value()，所以可以安全地放弃进入
            // continue 跳转到外层 for (;;) 的下一轮，重新等待门打开
            continue;
        }

        // 通过第二次检查后，线程获得进入受保护区域的资格
        // 即使 Gaia 随后关闭 gate，也必须等待本次调用完成并减少 active
#endif
        uint64_t start = now(); // 获取目标函数调用开始时间
        int value = target_value(); // 执行被热补丁修改的测试目标函数
        uint64_t end = now(); // 获取目标函数调用结束时间

        // 检查返回值是否属于预期集合
        // 该检查只能识别异常返回值，不能证明不存在新旧版本调用重叠等一致性问题
        if (value != 10 && value != 99) // 要么是原本的 10，要么是修改后的 99，否则说明观察到了非预期返回值
        {
            // 输出错误返回值到标准错误流
            fprintf(stderr,"INVALID %d\n",value);

            // 异常终止整个进程，便于测试驱动发现错误
            abort();
        }

#if LONG_CALL
        // 记录线程编号、开始时间、结束时间及返回值
        // 注意：这里输出的是两个纳秒时间戳，并非直接输出耗时
        // 测试分析器可以计算 end-start，并比较不同版本调用的时间区间
        printf("CALL %ld %lu %lu %d\n",id,start,end,value);
#endif

        // 本次调用及其相关记录操作完成后，增加总调用计数
        atomic_fetch_add(&calls, 1); // 增加调用计数

#ifndef GAIA_BASELINE
        // 退出受保护区域，活动调用计数减 1
        // Gaia 只有确认 active = 0 且全部相关线程已停止，才允许执行严格模式的代码切换
        // 因此 active 覆盖的区间不止 target_value() 本身，还包括后续结果检查和计数操作
        atomic_fetch_sub(&gaia_safepatch_state.active, 1); // 受保护调用完成，活跃计数--
#endif
    }

    // 当前 for (;;) 没有正常退出路径，此处实际上不可达
    return 0;
}

/// @brief 主线程入口，初始化测试环境、创建工作线程并定期报告调用数量
/// @return 程序退出状态，0 表示正常结束，非零值表示失败
int main(void) {
    // 将父进程设置为允许进行 ptrace 追踪的进程
    // 用于适配启用了 Yama ptrace 访问限制的测试环境
    // 注意：这不会绕过容器权限或所有其他 ptrace 安全限制
    // Test harness is the authorized external tracer for this test.
    prctl(PR_SET_PTRACER, getppid(), 0, 0, 0);

    // 将标准输出设置为行缓冲模式
    // 每输出一行 READY、CALL 或 COUNT 日志，便于测试驱动及时读取
    setvbuf(stdout, NULL, _IOLBF, 0);

    // 保存 8 个工作线程的 pthread_t 标识
    // pthread_t 是 pthread 库的线程标识，不等同于 Linux 内核 TID
    pthread_t threads[8]; // 8 个工作线程

    // 初始化同步屏障，参与者数量为 9
    // 包括 8 个工作线程和 1 个主线程
    // 当全部 9 个线程到达屏障后，才能继续执行
    pthread_barrier_init(&barrier, NULL, 9);

    // 依次创建 8 个工作线程
    for(long i = 0; i < 8; i++)
    {
        // pthread_create 参数依次为：
        // 线程标识存放地址、线程属性、线程入口函数、传入入口函数的参数
        // 使用默认线程属性，每个线程执行 worker()
        // 将循环编号 i 转换为指针形式传递，由 worker() 再转换回来
        if(pthread_create(&threads[i], 0, worker, (void*)i))
        {
            // pthread_create 返回非零值表示创建线程失败
            return 1;
        }
    }

    // 向测试驱动报告目标进程已经创建线程并准备就绪
    // READY 后依次输出：
    // PID、目标函数地址、协议状态地址、风险释放标志地址、总调用计数地址
    // 测试驱动利用这些地址定位目标机器码和共享状态
    printf("READY %d %p %p %p %p\n",getpid(),(void*)target_value,(void*)&gaia_safepatch_state,(void*)&gaia_risk_release,(void*)&calls);

    // 主线程也到达同步屏障，等待全部工作线程准备完成
    // 屏障释放后，工作线程开始执行各自的测试循环
    pthread_barrier_wait(&barrier);

    // 主线程不直接调用目标函数，而是周期性报告系统运行情况
    // 每秒输出一次当前单调时钟时间戳及已完成调用总数
    // 测试驱动可以根据 COUNT 日志观察调用吞吐量
    for (;;) { sleep(1); printf("COUNT %lu %lu\n",now(),atomic_load(&calls)); }
}
