# 3 分 50 秒讲解与录屏节奏

总计 **230 秒**：target.c 40 秒 + 驱动目的 30 秒 + 六格演示 120 秒 + 结论 40 秒。
六格平均只有 20 秒，因此“每格不超过 30 秒”还不够；正式录制按每格 20 秒排程。
以下时间表是讲解计划，不是脚本强行插入的停顿或自动按键。

## 录制前准备，不计入视频

从仓库根目录执行一次构建，避免将冷编译计入六格时长：

```bash
cargo build --offline --manifest-path tests/safepatch/Cargo.toml --bin safepatch-concurrent-export
```

主终端建议至少 100 列、28 行，准备好下面六条命令或终端历史。
另一个终端提前运行：

```bash
bash tests/safepatch/concurrent-export/monitor.sh
```

该监视器跟随 `/tmp/gaia-3x2-interactive/current`，六个场景自动切换。不要把输出目录
当成命令执行。旧监视入口也可继续按模式使用：

```bash
bash tests/safepatch/demo/monitor.sh compatible
```

不传根目录时会在旧目录和新 3×2 目录中选择该模式最近发布的会话；显式传入根目录
则只监视指定位置。若使用自定义根目录，两个终端应一致：

```bash
GAIA_CASE_ROOT=/tmp/gaia-video bash tests/safepatch/concurrent-export/show.sh aligned compatible
bash tests/safepatch/concurrent-export/monitor.sh /tmp/gaia-video
```

`GAIA_CASE_OUTPUT_DIR` 仍可指定单个会话位置；`GAIA_CASE_ROOT` 用于发布供监视器
跟随的指针。会话目录包含 session.json、files、真实事件、结果和 runtime.log。

## 0:00–0:40 展示 target.c

只展示三处代码，不逐行解释线程基础设施。

| 视频时刻 | 代码位置与讲解重点 |
|---|---|
| 0:00–0:12 | `render_document` 与 `export_document`：前者只排版，后者覆盖完整作业 |
| 0:12–0:25 | 共用的 `print_document`：真实写文件、fclose、rename；正式提交是业务完成边界 |
| 0:25–0:40 | `renderer` 的两个分支和 `printer`：四任务真正并发；async 计数随任务移交，提交后才减少 |

口述参考：

> 两种业务都有四个作业同时执行。aligned 中，一个补丁目标函数负责排版、写出和
> 最终提交；async 中，目标函数只排版，打印交给其他线程。两者共用写文件代码。
> 我们统一检查：切换返回以后，是否还有旧文档提交。协作计数覆盖完整任务，
> 异步交接时也不能提前减掉。

## 0:40–1:10 解释驱动做什么

展示 driver.rs 的“提交旧任务—安装—新任务—恢复”主流程，不展开 FFI 或 ELF 细节。

| 视频时刻 | 内容 |
|---|---|
| 0:40–0:50 | 同时提交 0–3 四个旧作业，等四个 BEGIN 后触发安装 |
| 0:50–1:00 | 安装后提交 4–7，验证维护入口拒绝导出；恢复后提交 8–11 验证正常导出 |
| 1:00–1:10 | 对比真实函数结束、文档提交和安装返回时间，验证四个区间确有重叠；监视器独立观察文件 |

口述参考：

> 驱动同时发出四个旧作业，等它们都开始后才安装补丁。随后提交四个新作业验证
> 入口已经改变，再恢复代码并验证四个作业正常导出。判定使用真实业务时间戳、
> 文件内容和代码读回，不使用预先写好的结果。

## 1:10–3:10 六格演示

先展示 Compatible 能解决的 aligned，再展示它覆盖不到的 async。三个路径
都使用相同的快速配置：**排版 3000 ms，写出 2000 ms**，这是 show.sh 的新默认。
C 程序和自动验收默认耗时未改；补丁算法与安全判据未改。

| 视频时段 | 组合 | 只观察这一点 |
|---|---|---|
| 1:10–1:30 | aligned / Base | 安装先返回，旧文档之后提交 |
| 1:30–1:50 | aligned / Compatible | busy 等待完整旧函数结束，旧文档先提交 |
| 1:50–2:10 | aligned / Cooperative | active 从 4 排空，旧文档先提交 |
| 2:10–2:30 | async / Base | 旧排版仍活动时返回，旧文档之后提交 |
| 2:30–2:50 | async / Compatible | 等待旧排版退出，但下游仍提交旧文档 |
| 2:50–3:10 | async / Cooperative | 等待最终提交后才返回 |

按这个顺序运行，默认参数即可：

```bash
bash tests/safepatch/concurrent-export/show.sh aligned base
bash tests/safepatch/concurrent-export/show.sh aligned compatible
bash tests/safepatch/concurrent-export/show.sh aligned cooperative
bash tests/safepatch/concurrent-export/show.sh async base
bash tests/safepatch/concurrent-export/show.sh async compatible
bash tests/safepatch/concurrent-export/show.sh async cooperative
```

每格本地 20 秒操作目标：

| 本格时间 | 按键与实际动作 |
|---|---|
| 0–1 秒 | 执行命令，Enter 启动服务 |
| 1–2 秒 | Enter 同时提交四个旧任务 |
| 2–3 秒 | 看到四个开始事件后，立即 Enter 安装；不要在此处解释原理 |
| 3–7 秒 | 观察真实状态和文件；安装返回时刻因路径不同而不同 |
| 7–8 秒 | Enter 提交四个新任务，看到拒绝导出 |
| 8–9 秒 | 旧文档全部完成、恢复提示出现后，Enter 恢复并验证四个作业 |
| 9–15 秒 | 等待恢复作业真实完成，Enter 关闭服务 |
| 15–20 秒 | 指出这一格的时间先后关系，切换到下一条命令 |

脚本每步仍需手动 Enter，不会按此表自动运行。渲染与写出总计 5 秒，恢复验证
再用约 5 秒；四任务并行，因此不是 4×5 秒。其余时间留给按键、编译缓存检查和
命令切换。时间表不保证任意慢速机器都能达标，录制前应在实际设备彩排。

特别注意：提交旧任务后，安装应在 **3 秒排版结束前**触发。讲解已放在前 70 秒，
不要在第三个提示处停留长讲。错过这个窗口时重开该格，不能把时序断言失败当成功。
如手速需要，可统一改成 `4000 1500`，但应重新彩排总时长，不能只改某一个模式。

每格最多一句说明即可，避免挤占下一次按键：

- aligned/Base：“安装返回了，但旧文档还会提交。”
- aligned/Compatible：“旧函数包含整个业务，所以这次能等完。”
- aligned/Cooperative：“计数归零后切换。”
- async/Base：“这次连旧排版都没有等。”
- async/Compatible：“排版等完了，打印任务还在后面。”
- async/Cooperative：“计数跟到最后提交，才宣布切换。”

## 3:10–3:50 解释现象和结论

前 20 秒指向 3×2 矩阵，后 20 秒解释适用条件与来源。

> 在完整函数覆盖业务的第一组里，Compatible 和 Cooperative 都能保证这个案例
> 的旧文档先提交，Base 做不到。第二组里，旧函数退出后还有异步打印，Compatible
> 只能排空函数调用，无法据此排空下游任务。Cooperative 通过正确移交活动计数，
> 把保护范围延伸到最终提交。非协作扫描来自 openEuler SysCare；我们的工作是
> 适配、集成、安全审计和实验验证。这里证明的是两个明确模型，不能推广成任意
> 程序的普遍一致性保证。

“失败”须解释为“不满足返回后无旧文档提交这项要求”，不等于进程崩溃或补丁字节
写入失败。六格都要展示新任务拒绝和恢复验证，不能为了两分钟而删掉安全验证。

## 彩排验证

```bash
python3 tests/safepatch/concurrent-export/verify-interactive.py /tmp/gaia-rehearsal --sequential
```

按六格顺序真实发送 Enter，使用 3 秒 + 2 秒业务参数。timing.json 记录总耗时，
validation.json 记录每格耗时、真实重叠、旧文档提交数和恢复结果。自动彩排按键仅
延迟约 80 ms，实测时间是功能执行耗时，不是包含人工讲解的正式视频时长。

监视器切换会话前会补读上一目录，避免快速切换漏掉最后一批文件。监视行时间是
“观察到变化”的时刻；精确业务提交时刻仍看主终端或原始事件，不将轮询时刻当作
fclose/rename 的发生时间。

最新顺序彩排：六格功能总耗时 **63.37 秒**，各格 **10.45–10.71 秒**，全部满足
业务矩阵和恢复检查；监视器连续跟随六场并观察到各场旧文档与恢复文档。
这为 120 秒演示段预留约 56 秒供人工操作和切换，不把自动按键耗时当成人工讲解耗时。
[原始彩排记录](evidence/rehearsal/rehearsal.log)、[计时](evidence/rehearsal/timing.json)、
[监视器记录](evidence/rehearsal/monitor.log) 可复核。
