# SysCare Compatible 集中验收

本轮仅修正验收测量和报告，未扩展运行时算法。`target.c`、`cooperative.h` 与
Git HEAD 逐字节一致，SHA256 保存在 [实测记录](../tests/safepatch/measured-syscare-results.json)。
非协作机制来自 openEuler SysCare 1.2.2-8.oe2403sp4；本项目工作是源码移植、
Rust FFI 集成、安全错误处理审计与实验验证，不将栈扫描算法归为原创。

## 相同 9 线程业务的实测

重新构建 demo，Control、Base、Compatible、Cooperative 各运行一次，再各重复 10 次。
以下统计来自这 40 次重复运行；Control 两个文档均正常生成。

| 模式 | 旧文件相对安装返回的顺序 | 重复结果 | 新入口与恢复 |
|---|---|---|---|
| Base | 返回后约 799.96–800.32 ms 才开始创建旧文件 | 10/10 复现维护要求违规 | 新作业不导出，恢复后第三个作业正常导出 |
| Compatible | 旧文件在返回前约 4.05–7.97 ms 已关闭 | 10/10 未观察到旧文件跨越返回 | 同上；旧调用活动时 30 ms 请求拒绝，再等待后安装 |
| Cooperative | 旧文件在返回前约 0.84–3.62 ms 已关闭 | 10/10 满足本例保护区间的顺序要求 | 同上；保护计数覆盖排版至文件关闭 |

**本案例尚未实测出 Compatible 的业务一致性违规。也没有建立其业务排空保证。**
不能把 Compatible 的 `consistency_complete=false` 解释为观测到违规：这个旧字段
表达“未声明业务一致性”，现在另行记录 `business_observation` 表达实际时序。

机器码读回只是其中一项检查。驱动还通过正常 stdin 请求验证实际执行：
旧作业确实创建 `document-0.txt`，安装后的作业 1 不创建文件，恢复后的作业 2
重新创建内容正确的文件。补丁前后业务程序均存活。

## Compatible 实际等到了什么

只读检查器直接编译并调用当前 C bridge 的 stop/check/release，不安装补丁、不写
业务内存、不改寄存器或执行位置。它单独启动相同未改动的普通文档服务，捕获：

- 108 次 busy 快照，每次确认 9 个线程被暂停。
- 导致 busy 的线程 RIP 位于 libc 的 `clock_nanosleep`。
- 108 次均由栈内 `0x401479 = render_document+9` 命中，反汇编确认这是
  `call layout_document` 的下一条指令地址。它确实检查到了睡眠中的旧调用者。
- 最后一次 busy 快照约在 803.14 ms，旧文件尚不存在；首次 clean 约在
  810.67 ms，文件已存在。两个快照相隔约 7.53 ms。

因此，旧调用等待有明确的栈内证据。另一方面，检查器没有采样到
`render_document` 返回至 `fopen/fclose` 完成之间的短窗口。业务源码中创建文件
明确发生在 render 返回之后；被检查的函数范围没有包含完整 worker，也没有
跟踪队列或文件关闭协议。**当前文件先关闭的观测不能证明检查器以文件关闭为判据。**
快照间隔与业务写入短窗口的交错仍可能影响结果；不能仅凭重复 10 次未违规就排除它。

同一业务案例目前证明了 Base 的反例，以及两种改进路径均可完成安全条件下的安装和
恢复；它尚不能作为 Compatible 与 Cooperative 在业务副作用上必然不同的实验反例。

## 测量方法与判定边界

本轮修正了两处验收问题：

1. `switch_return_ns` 现在紧随交换函数返回采样，在校验、打印和额外文件读取之前。
   它是调用方对返回边界的紧邻观测，仍存在调用返回到取时钟的测量间隙。
2. 原提示只依据“开始写入早于返回”就声称“已经写完”；现在分别比较开始和结束，
   记录完成在前、创建在后、活动区间跨越返回三种情况。Compatible 不强制预期顺序。

业务的 begin 在 `fopen` 前，end 在 `fclose` 后，均与驱动使用 CLOCK_MONOTONIC。
不是用日志接收先后或混用墙上时钟的文件 mtime 判断；实际文件及内容另行检查。
`begin > return` 证明随后才发起文件创建；`end < return` 表示文件关闭发生在
观测返回时刻之前。区间跨越返回不能据此确定具体哪次写操作发生在返回之后，
但也不能确认已经排空。文件关闭不等于存储介质持久化，本测试不声称 fsync 保证。

## 故障安全验收

重新执行 `run-compat.sh`，6 项测试全部通过。覆盖线程 SEIZE/INTERRUPT/GETREGS/
DETACH 失败、已有 SIGSTOP、实际 bad-fd pread、短读取、未知栈映射、安装和恢复的
写失败/读回失败、部分写入以及持续回滚失败。检查错误不会授权写入；能够回滚时
校验原字节；持续回滚失败时保持线程停止，测试显式修复后才恢复执行。

DETACH 失败可能已释放部分线程，因此不能笼统说所有错误都保证原代码不变。
该路径返回未完成错误并携带原字节，不声明一致性完成；Gaia 保留恢复记录。
持续失败不等于自动恢复，仍受 tracer 存活、权限和内核状态限制。

## 可复核证据与命令

- [最新实测记录](../tests/safepatch/measured-syscare-results.json)：旧三轮记录保留在 previous_document_demo。
- [时序汇总](../tests/safepatch/syscare-evidence/timing-summary.json)、[40 轮原始时间戳](../tests/safepatch/syscare-evidence/acceptance-repeat-results.json)。
- [只读栈快照](../tests/safepatch/syscare-evidence/native-stack-audit.json)、[文档 demo 日志](../tests/safepatch/syscare-evidence/document-demo.log)。
- [故障测试日志](../tests/safepatch/syscare-evidence/fault-tests.log)，每模式逐轮日志同目录 acceptance-*.log。

```sh
bash tests/safepatch/demo/run.sh /tmp/gaia-syscare-acceptance
python3 tests/safepatch/demo/repeat.py /tmp/gaia-syscare-acceptance 10
python3 tests/safepatch/demo/inspect-native.py /tmp/gaia-syscare-acceptance/plain /tmp/gaia-syscare-acceptance/stack-audit.json
bash tests/safepatch/run-compat.sh /tmp/gaia-syscare-acceptance/faults
```

PPT 可表述为：非协作路径基于 openEuler SysCare，完成多线程适配、集成及安全审计；
协作路径通过显式保护区间表达栈地址扫描无法获知的业务完成条件。本例验证了旧调用
等待及代码恢复；Compatible 未观察到业务违规，但没有证明异步业务排空。
