# 多线程文档导出对比

业务源码 target.c 和 cooperative.h 保持原样。8 个 worker 和 1 个主线程运行真实文件导出。

```sh
bash tests/safepatch/demo/run.sh /tmp/gaia-syscare-demo
python3 tests/safepatch/demo/repeat.py /tmp/gaia-syscare-demo 3
```

- Control：两个作业都生成文档。
- Base：补丁立即返回，旧文档在返回之后才开始创建；恢复后第三个文档正常导出。
- Compatible：先以 30ms 超时验证活动旧调用被拒绝且代码不变，再等待旧 render_document
  调用结束后安装。新作业不导出，恢复后第三个文档正常导出。仅声明原生栈检查完成。
- Cooperative：计数覆盖排版至文件关闭，断言旧文件写完早于切换返回。

Compatible 的栈检查覆盖 RIP 位于 nanosleep 时的旧调用者，不只是入口覆盖区间。
当前这次交错通常在文件写完之后才采样到干净栈；不能推导任意调度下目录稳定，
不对 Compatible 断言业务排空，也不声明异步队列或下游工作已经完成。
详细边界和失败语义见 [移植说明](../../../gaia-xdp/src/safepatch/syscare/README.md)。

驱动通过 stdin 提交作业，不改寄存器或业务内存来制造时序。实际机器码读回、
文件内容、单调时钟以及恢复后执行均校验；输出 files.result.json 在业务目录外。
历史 example-* 和 measured-results.json 不代表当前版本；本次 repeat-results.json
和 runtime.log 才对应当前实测。未验证 HTTP/eBPF 端到端流程。

集中验收的新时序统计、只读栈快照和 PPT 表述边界见
[验收结论](../../../docs/safepatch-acceptance.md)。当前重复 10 次没有观察到 Compatible
返回后生成旧文件；这不是其业务排空保证。`consistency_complete` 表达保证声明，
`business_observation` 另行表达实测时序。返回时间已改为交换调用返回后立即采样。

只读检查旧调用命中原因（不安装补丁）：

```sh
python3 tests/safepatch/demo/inspect-native.py /tmp/gaia-syscare-acceptance/plain /tmp/gaia-syscare-acceptance/stack-audit.json
```
