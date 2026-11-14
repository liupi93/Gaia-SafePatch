/* 可选协作接口：业务程序只需包住“排版到文件提交”的完整区间。 */
#ifdef COOPERATIVE
struct
{
    _Atomic uint64_t magic;
    _Atomic uint64_t gate;
    _Atomic uint64_t active;
} gaia_safepatch_state = {0x4741494153503031, 0, 0};
#define DEMO_STATE ((void *)&gaia_safepatch_state)
#else
#define DEMO_STATE ((void *)0)
#endif

static void enter_region(void)
{
#ifdef COOPERATIVE
    for (;;)
    {
        while (atomic_load(&gaia_safepatch_state.gate))
        {
            sched_yield();
        }

        atomic_fetch_add(&gaia_safepatch_state.active, 1);
        if (!atomic_load(&gaia_safepatch_state.gate))
        {
            return;
        }
        atomic_fetch_sub(&gaia_safepatch_state.active, 1);
    }
#endif
}

static void leave_region(void)
{
#ifdef COOPERATIVE
    atomic_fetch_sub(&gaia_safepatch_state.active, 1);
#endif
}
