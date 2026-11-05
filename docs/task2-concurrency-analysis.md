# 任务二：热补丁并发一致性问题分析

## 原理与问题

Gaia 的 `override_return` 会解析运行时符号地址，用 `PTRACE_ATTACH` 停止配置 PID，再以 `PTRACE_POKETEXT` 把函数入口改成 8 字节返回桩。x86_64 返回桩是 `mov eax, imm32; ret; nop; nop`。这能改变后续调用，却不提供“一个业务请求只用一个版本”的语义。

## 可重复实验

`experiments/concurrency/gaia_consistency_demo.c` 创建多个线程，每个线程代表一个请求：先调用旧函数；barrier 保证所有线程首次调用已完成但请求未结束；Gaia 此时应用补丁；放行线程再次调用并分类。

| 分类 | before | after | 含义 |
| --- | --- | --- | --- |
| `OLD_ONLY` | `input+100` | `input+100` | 全程旧语义 |
| `NEW_ONLY` | `200` | `200` | 全程新语义 |
| `MIXED` | `input+100` | `200` | 同一请求混用新旧版本 |
| `UNEXPECTED` | 其他 | 其他 | 必须调查 |

正式 openEuler 实验应至少重复 30 轮，保存原始日志、Git SHA、内核版本、线程数和每轮统计。

## 结论

出现 `MIXED` 即证明：函数替换成功不等于业务一致；单次 ptrace 修改没有请求事务边界；仅等待目标函数活跃调用数归零仍可能过早切换，因为请求可以在两次调用之间保持活跃。

后续安全协议至少需要：

```text
关闭新请求入口 -> 等待旧业务请求归零 -> 确认函数无活跃栈帧
-> 应用并校验补丁 -> 以新版本开放请求
```

Uprobe/Uretprobe 可感知函数进入/退出，但业务边界仍需额外探针、请求 ID/TLS 或应用协作。无法识别业务边界时，只能承诺函数级 quiescence，不能承诺请求级一致性。

## 边界

- 真实二进制可能内联、剥离符号或使用复杂调用约定；
- 8 字节入口覆盖是架构相关实现，x86_64 结果不能推断 aarch64；
- 停止配置 PID 不等于协调线程组内所有线程；
- 本实验验证 `override_return`，不代表 `replace_function` 动态库注入路径已验证；
- WSL2/CentOS 结果不能替代 openEuler SP4 VM；
- 当前交付完成任务一和任务二，不声称已完成任务三。

| 日期 | Git SHA | openEuler / 内核 | 线程数 | 轮次 | MIXED/总数 | 日志路径 |
| --- | --- | --- | ---: | ---: | ---: | --- |
| 2026-11-06 | `0c6879f` | openEuler 24.03 LTS-SP4 / `6.6.0-159.4.3.154.oe2403sp4.x86_64` | 8 | 30 | `240/240` | `experiments/concurrency/results/openeuler-sp4-30-rounds.log` |

30 轮均以退出码 0 完成，每轮均为 `SUMMARY total=8 mixed=8`，总计为
`REPEAT_SUMMARY rounds=30 mixed=240 total=240`。原始日志 SHA-256 为
`e8dac6911f2ff27a2a9ead829bb2ac4660f17a92a3e7c03829a32f3c3334186a`。

## 本机预验证（不作为 openEuler 正式验收）

2026-11-03 在 Docker Desktop 的 WSL2 6.6.87.2 内核、CentOS Stream 9 用户空间中完成预验证：Gaia release 构建成功，eBPF 对象及文件/进程/网络/热补丁探针加载成功；8 线程实验连续运行 30 轮全部通过，汇总为 `REPEAT_SUMMARY rounds=30 mixed=240 total=240`。该结果证明实验链路可用，但根据小组统一要求，不能替代 openEuler 24.03 LTS SP4 虚拟机结果。
