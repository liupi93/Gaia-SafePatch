# Gaia-SafePatch 课堂交互演示

> 本页原用例只有一个活动旧任务，只用于操作演示，不再作为多任务并发或三模式
> 业务排空差异的验收证据。真正四任务并发的对比见
> [并发文档流水线](../concurrent-export/README.md)，其 3×2 自动验收与六个 Enter 入口独立于本页旧交互程序。

终端仅显示操作、真实状态和业务事件。下面的原理、预期现象与结论供演讲者口述，
程序不会将这些内容打印到主终端。

## 1. 环境准备与启动命令

使用 openEuler 24.03 LTS SP4、Linux x86_64，准备仓库指定的 Rust 工具链、C 编译器、
Git 和 Python 3。运行用户需要有追踪自己子进程及读取 `/proc/PID/mem` 的权限。
在仓库根目录运行；入口会离线构建驱动并编译同一份文档业务源码。

```bash
bash tests/safepatch/demo/interactive.sh base
bash tests/safepatch/demo/interactive.sh compatible
bash tests/safepatch/demo/interactive.sh cooperative
```

三个模式默认均使用 **30 秒**任务。建议录屏终端至少 100 列、28 行。
交互终端使用少量颜色和两行固定区域：一行状态、一行当前操作提示。
`NO_COLOR=1` 可关闭颜色，原地更新仍保留。重定向到文件或 `TERM=dumb` 时输出无
控制序列的事件日志，状态只记录变化，不持续打印检查次数；完整检查保存在证据文件。

参数依次是模式、任务毫秒、产物根目录、补丁等待上限毫秒。任务时长范围为
1–3600000 毫秒；等待上限默认是任务时长加 10000 毫秒。例如需要一分钟讲解窗口：

```bash
bash tests/safepatch/demo/interactive.sh compatible 60000 /tmp/gaia-video
```

三模式比较时须使用相同的时长和业务输入。默认 30 秒是业务真实耗时，并非界面
暂停、假进度或专门冻结旧调用；原自动化 demo 仍使用默认 800 毫秒。

### 独立文件监视终端

在第二个终端运行对应命令，先运行监视器也可以：

```bash
bash tests/safepatch/demo/monitor.sh base
bash tests/safepatch/demo/monitor.sh compatible
bash tests/safepatch/demo/monitor.sh cooperative
```

自定义产物根目录时，两个终端传入相同的根目录：

```bash
# 主终端
bash tests/safepatch/demo/interactive.sh compatible 30000 /tmp/gaia-video
# 第二个终端
bash tests/safepatch/demo/monitor.sh compatible /tmp/gaia-video
```

监视器读取 `current-模式` 指向的最近一次会话，切换新会话时自动跟随。
它每 250 ms 检查真实 `document-*.txt` 的文件名、大小、mtime、inode，只在新增、
变化或移除时打印。观察到新增不等于文件已经关闭；关闭事件由主终端业务日志提供。
监视器的时间基准与当前会话相同，打印的是文件变化被观察到的时刻。

查看当前真实目录或只取一次文件快照：

```bash
readlink -f /tmp/gaia-document-interactive/current-compatible
bash tests/safepatch/demo/monitor.sh compatible --once
```

上面第一条返回的路径供 `ls`、文件管理器或 `watch` 使用，不是可执行命令。
主终端不会打印绝对路径。服务结束后文件仍保留；最后一次会话若在启动前取消，
监视器看到空目录也是实际状态。

## 2. 实际操作步骤及按键时机

每次只按一次 Enter，不要粘贴多行回车。提前输入的 Enter 不会授权后续步骤。
任务事件出现时，输入提示保持在底部，不需要重新输入命令。

| 屏幕提示 | 操作与时机 |
|---|---|
| `[Enter] 启动服务` | 按 Enter；确认出现 PID、实际线程数量和模式 |
| `[Enter] 提交旧任务 #0` | 按 Enter；等待“任务 #0 开始处理” |
| `[Enter] 安装维护补丁` | 在旧任务结束前按 Enter，观察真实等待状态 |
| `[Enter] 提交新任务 #1` | 安装成功后按 Enter；观察任务被维护版本拒绝导出 |
| `[Enter] 恢复补丁` | 按 Enter；恢复成功后程序提交任务 #2，验证正常导出 |
| `[Enter] 关闭服务` | 任务 #2 关闭文件后按 Enter；服务停止，文件保留 |

默认从旧任务开始到关闭约有 30 秒。建议在第三步停留讲解 5–10 秒后触发补丁，
录屏前完成较长的背景介绍。需要更多时间时先增大真实任务时长。
若讲解超过时长，终端正常显示旧文件已经关闭；此后检查可以立即通过，程序不会
伪造 busy 或阻止任务完成。该轮不能用于展示活动旧调用等待，应另开新会话。

等待安装/恢复时 Enter 不会推进到下一步。发生拒绝或失败时停止后续任务提交，
不会显示安装成功，也不会继续新版本验证。输入 EOF 会停止演示并关闭自己启动的服务。

## 3. 演讲者每一步应讲解的内容

| 步骤 | 演讲者说明 |
|---|---|
| 启动 | 同样是 8 个 worker 加 1 个主线程的文档服务，Base/Compatible 无业务协作 ABI，Cooperative 使用已有可选 ABI |
| 提交旧任务 | 同一业务先排版，再创建并关闭文档；本地排版替身使用真实 nanosleep 模拟耗时。等待不是 UI 人工延时 |
| 安装 | Base 使用原版写入器；Compatible 基于 openEuler SysCare 的原生栈地址扫描；Cooperative 根据门禁和活动计数协调保护区间 |
| 提交新任务 | 新入口返回不导出，真实业务日志报告拒绝，输出目录没有 document-1.txt。读回机器码之外还验证了入口实际执行 |
| 恢复 | 同一路径恢复原始字节，任务 #2 重新走业务流程并生成 document-2.txt，证明服务恢复工作 |

Compatible 状态中的 `检查次数` 来自已有检查回调，`busy` 来自 C 扫描结果。
本演示一次只保护 `render_document`，因此 `render_document@0x...` 是该唯一受影响
函数的真实入口地址，**不是当前 RIP，也不是声称读取到了某个具体栈槽地址**。
不增加第二套扫描器，也不改变原来的检查、重试或写入算法。

Cooperative 的 `gate` 和 `active` 来自直接读取目标已有 ABI；每 250 ms 采样，
状态值不变时不重绘相同文本，所有采样仍写入证据。两个原子字段不构成事务快照。
Base 不会显示并不存在的栈检查或门禁状态。

项目应将非协作机制归功于 openEuler SysCare。Gaia 的工作是适配、集成、安全审计
和实验验证，不能将上游栈扫描算法表述为原创。

## 4. 三种模式的预期现象

| 模式 | 旧任务仍活动时触发安装 | 输出目录及恢复 |
|---|---|---|
| Base | 通常很快返回；没有完整旧调用排空保证 | 可观察到安装返回后 document-0.txt 才生成；新任务不导出，恢复后任务 #2 导出 |
| Compatible | 真实 busy、检查次数增长，干净快照后安装；超时则拒绝 | 现有案例常在旧文件已关闭后才返回，但这不是业务排空保证；恢复后任务 #2 导出 |
| Cooperative | 门禁关闭，active 随旧保护区间完成归零，然后安装 | 本例保护区间包含文件关闭，旧文档在安装返回前关闭；恢复后任务 #2 导出 |

这些是有条件的现象，终端不预先宣布预期结果。操作者按键过晚、系统调度差异或
检查保守误报都可能改变实际等待时长。Compatible 不保证异步任务、队列、下游工作
或函数返回后的副作用已经结束；Cooperative 的保证取决于完整保护区间与所有入口
遵守协议，不能自动扩大到区间外业务。

## 5. 重要技术边界与异常处理

- 界面使用从会话启动计时的 `+秒.毫秒s`，例如 `+6.239s`。
  `events.jsonl` 同时保留完整 CLOCK_MONOTONIC 纳秒和 `relative_ns`；
  `session.json` 保存统一时间原点。
- 文件关闭时间取自业务在 fclose 后记录的事件；安装/恢复返回时间紧随调用返回采样。
  异步日志到达顺序不等于事件发生顺序，应比较原始时间。关闭不等于介质持久化。
- Compatible 控制台最多约每 250 ms 更新一行；全部检查事件、返回码及函数入口地址
  仍保存。Cooperative 的重复采样也完整保存。终端不打印最终分析报告。
- 每个独立会话目录保留 `events.jsonl`、`session.json`、`summary.json`、原始
  `runtime.log`、构建日志及 service；业务文件单独放在 files 子目录。
  summary 保留实际任务、时间戳及机器可读观察值，不在终端生成讲稿。
- `summary.observation` 的 before/after/crosses 指本次文件活动区间与安装返回的关系；
  `consistency_scope` 为 `unclaimed`、`native_stack_check` 或 `cooperative_region`，
  不把一次观察结果扩大为普遍保证。
- 超时/不支持条件显示“补丁安装拒绝”；其他错误显示安装或恢复失败，详细原因只进证据。
  拒绝、失败、输入关闭均非零退出。程序只管理自己创建的演示服务。
- 可演示真实超时拒绝：

  ```bash
  bash tests/safepatch/demo/interactive.sh compatible 30000 /tmp/gaia-video 100
  ```

  及时提交旧任务并安装即可观察；旧任务已经结束时不会为了演示而制造拒绝。
- 原有持续回滚或 detach 失败限制仍然成立，UI 不新增安全保证，也不绕过检查。
  UI 不调用 waitpid/try_wait 抢占补丁线程的 ptrace 暂停事件。

### 验证与实际终端示例

```bash
# 三个 5 秒场景，外加拒绝、旧任务提前完成、输入 EOF
python3 tests/safepatch/demo/verify-interactive.py /tmp/gaia-classroom-quick
# 三个 30 秒场景，模拟讲解 6 秒再按安装；各自使用独立 PTY 和业务进程
python3 tests/safepatch/demo/verify-interactive.py /tmp/gaia-classroom-recording --recording
```

验证包含真实文件、入口生效、恢复、命中函数地址、相对时间、ANSI 固定区域重绘、
事件不被覆盖或重复进入滚动区、独立监视器观察到新增文件。原始终端流及还原的
屏幕文本一并保存。原有 show.sh、repeat.py 与故障测试继续使用原自动路径。

本次验证记录：[measured-interactive-results.json](measured-interactive-results.json)。
三个 30 秒场景实际执行了六秒讲解窗口；另外通过三个 5 秒场景、超时拒绝、旧任务
提前结束、EOF、Ctrl-C 终端恢复和无 ANSI 的管道输出验证。独立监视器观察到了任务
#0/#2 文件，没有观察到被拒绝任务 #1 的文件。原自动 demo、四模式各 3 次 repeat
及 6 项 Compatible 故障测试通过。算法、安全判据和业务源码的哈希与本轮开始时一致。

[三段实际终端输出](interactive-evidence/classroom/examples.md) 包含选取的一次状态行；
状态在现场原地刷新，不逐条进入事件滚动区。完整 ANSI 终端流、还原后的屏幕文本、
原始事件、摘要及文件监视日志都保存在该目录。
