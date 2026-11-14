# 3 × 2：四任务并发文档导出

三种路径 × 两种业务模型，统一要求是：**补丁安装返回以后，不再有旧任务提交正式文档。**
表中的“通过/不满足”指这个业务要求，不代表机器码安装是否成功。

| 业务模型 | Base | SysCare Compatible | Cooperative |
|---|---|---|---|
| async：业务生命周期大于目标函数 | 不满足，返回后提交 4/4 | 不满足，返回后提交 4/4 | 通过，返回后提交 0/4 |
| aligned：完整作业与目标函数重合 | 不满足，返回后提交 4/4 | 通过，返回后提交 0/4 | 通过，返回后提交 0/4 |

## 两个模型如何对应真实代码

两者都同时提交四个旧任务，采用相同的 1 秒排版和 2.5 秒分块写出替身（交互演示
默认排版 3 秒、写出 2 秒）。共用 **同一个 `print_document` 实现**：写真实 `.part` 文件、
关闭、rename 为正式 `document-N.txt`。提交正式文件是业务完成边界。
任务数、内容、设备速度在三种路径间完全一致，业务代码不检测补丁模式。

**async：**

```c
enter_region();
accepted = render_document(20);  // 补丁目标，仅包含排版
if (accepted)
    push(printing_queue, job);  // 由其他线程完成打印
else
    leave_region();
// 打印 consumer 在文件提交后 leave_region()
```

四个排版线程并发调用 render_document；四个下游打印线程并发完成输出。
Compatible 可以排空旧 render 调用，却不能由此证明下游队列已经排空。
Cooperative 把未完成任务计数随 job 移交给 consumer，最终提交之后才减计数。

**aligned：**

```c
int export_document(job) {      // 补丁目标，完整有限作业
    layout();
    print_document(job);       // 含关闭及正式提交
    return 1;
}
enter_region();
accepted = export_document(job);
leave_region();
```

四个作业线程同时工作，每个线程内部串行排版、写出并提交。
Compatible 等到旧 export_document 全部退出时，四份旧文档也已经提交。
进程仍保留相同的九线程拓扑；另外四个下游线程在此模型中空闲。
并发判据来自四个实际工作区间的重叠，不以总线程数作替代。

入口补丁都使用已有的 `mov eax,0; ret; nop; nop`。async 中新调用不获准进入打印
队列；aligned 中新调用直接跳过整个导出作业。恢复均走原有对应路径。

## 六个 Enter 演示入口

从仓库根目录运行：

```bash
bash tests/safepatch/concurrent-export/show.sh async base
bash tests/safepatch/concurrent-export/show.sh async compatible
bash tests/safepatch/concurrent-export/show.sh async cooperative

bash tests/safepatch/concurrent-export/show.sh aligned base
bash tests/safepatch/concurrent-export/show.sh aligned compatible
bash tests/safepatch/concurrent-export/show.sh aligned cooperative
```

逐步按 Enter：启动服务 → 同时提交四个旧任务 → 安装补丁 → 提交四个新任务 →
恢复补丁并运行四个验证任务 → 关闭服务。默认排版 3 秒、写出 2 秒，请在四个旧任务
开始后尽快触发安装。如果旧任务在讲解时已经结束，时序断言会失败，不制造等待。

可通过第三、第四参数统一配置两个阶段的毫秒数，例如 `aligned compatible 30000 10000`。
终端复用简洁事件与单行状态布局；显示真实函数返回、写出开始、文件提交，Compatible
显示实际检查次数，Cooperative 采样实际 gate/active。不在终端输出分析报告。
作业事件由读日志线程实时展示，不等补丁返回后再回放；UI 不竞争 waitpid。

演示产物默认保存在 `/tmp/gaia-3x2-interactive/业务-模式-*`，不在主终端打印长路径。
可显式指定 `GAIA_CASE_OUTPUT_DIR=/tmp/my-case`；目录中包括 files、events-模式.log、
result-模式.json、runtime.log。使用一个新的目录，避免混入旧文件。

## 自动验收

```bash
# 运行完整六格，输出目录自动生成
bash tests/safepatch/concurrent-export/run.sh
# 指定一个新的目录
bash tests/safepatch/concurrent-export/run.sh /tmp/gaia-matrix
# 仅运行其中一个组合
bash tests/safepatch/concurrent-export/run.sh /tmp/gaia-one aligned compatible
# 真实 PTY 逐步 Enter 验证全部六个组合（使用较短业务时长）
python3 tests/safepatch/concurrent-export/verify-interactive.py /tmp/gaia-matrix-pty
```

自动驱动等待四个 BEGIN，检查请求时刻落在四个旧调用的共同活动区间内；打印阶段
也要求四个 PRINT_BEGIN 到文件关闭的区间有共同重叠。不满足并发条件就失败。

- Base 必须观察到四个旧目标调用跨越安装返回，且四个旧文档随后提交。
- Compatible 必须出现真实 busy，四个旧目标调用在返回前结束；async 随后仍提交
  四份旧文档，aligned 则不能再有旧文档提交。
- Cooperative 在安装前实际读到 active=4，返回后旧文档提交数必须为零。
- 每格随后提交任务 4–7，全部拒绝且无正式文件；恢复后任务 8–11 全部导出，
  内容逐行核对，原机器码读回一致，目标进程存活。

脚本退出成功表示六格**复现了各自预期的能力边界**，并不把 Base/async Compatible
的业务不满足隐藏为“业务成功”。完整时间戳和统计见 result-*.json，业务日志及
运行时日志一并保留。

这些结果仅证明两个模型下的明确业务要求。排版和打印使用本地耗时替身，不是
真实 PDF 引擎或打印机性能测量。Compatible 仍可能因保守误报拒绝安装，也不保证
任意自定义栈、协程或异步业务。非协作机制来源于 openEuler SysCare；本项目做
复用、集成、审计和实验验证，没有修改补丁算法来获得此矩阵。

旧的单活动任务交互用例仍保留，但不作为三模式并发能力对比的证据。

[最新实测矩阵](measured-results.json) 保存三轮自动重复、最终自动六格验收和六个
真实 PTY Enter 会话的结果；原始业务事件、运行时输出和终端流见 evidence/matrix。
所有组合都验证了新任务拒绝、恢复后实际导出和原机器码恢复。

[3 分 50 秒讲解排程与监视器命令](PRESENTATION.md)。六格目标各 20 秒，原理放在运行前讲，提交旧任务后应在 3 秒内触发安装。

独立终端跟随六格当前输出：

```bash
bash tests/safepatch/concurrent-export/monitor.sh
```
