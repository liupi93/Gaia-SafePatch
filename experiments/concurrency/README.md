# 任务二：多线程热补丁并发一致性复现实验

每个线程代表一个业务请求，同一请求调用 `pricing_rule` 两次。第一次调用全部完成后，请求仍处于进行中；此时通过 Gaia 将旧语义 `input + 100` 原地替换为固定返回 `200`，再允许第二次调用。

## 运行

先按 `docs/task1-openeuler-reproduction.md` 在 openEuler 24.03 LTS SP4 中启动 Gaia，然后执行：

```bash
sudo THREADS=8 bash experiments/concurrency/run_experiment.sh
```

API 地址不同时设置 `GAIA_API`。成功判据是退出码为 0，末尾出现类似：

```text
SUMMARY total=8 mixed=8
PASS: an in-flight request observed old and new function semantics.
```

逐请求结果保存在 `experiments/concurrency/build/experiment.log`。脚本结束时会删除它新增的 Gaia 目标；若中断后清理失败，应在 WebUI 删除失效 PID 的目标，或恢复实验前的配置快照。

统一 openEuler 24.03 LTS-SP4 虚拟机上的 30 轮正式结果保存在
`results/openeuler-sp4-30-rounds.log`，汇总为 `rounds=30 mixed=240 total=240`。

该实验说明：函数入口替换成功不等于请求级一致；即使切换瞬间目标函数活跃调用数为零，只要请求跨越多个调用，仍会混用新旧版本。完整分析见 `docs/task2-concurrency-analysis.md`。
