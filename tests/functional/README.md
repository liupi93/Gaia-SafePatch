# 任务四②：HTTP 集成与监控补测

这套脚本只增加测试目标和测试驱动，不修改 Gaia/SafePatch 实现。被测源码冻结为 `be3dd66ecc1aa4479a490df8cfb8caefa9c2c836`。

## 执行条件

- 专用 Linux x86_64 测试机，正式验收使用 openEuler；root、cgroup v2、BTF 和项目已构建的 release 程序。
- Python 3 标准库、GCC、Git、bpftool、GDB、systemd-run 和 coredumpctl。openEuler 的 `/usr/libexec/gdb` 也受支持。
- 不得同时运行另一个 Gaia：端口封禁程序挂在根 cgroup，独立配置不能隔离这个全局挂载点。脚本发现已有 Gaia 时退出。
- 预留本机回环端口 17892、测试端口 45444，以及 core 文件空间。脚本不修改全局 core_pattern、SSH、代理或 systemd-coredump 配置。

## 运行

将下列路径换成实际位置；输出目录必须不存在，保留预跑和正式结果，不能覆盖失败记录。

```bash
python3 tests/functional/run.py \
  --source /path/to/clean-be3dd66-source \
  --binary /path/to/clean-be3dd66-source/target/release/gaia-xdp \
  --output /path/to/new-results \
  --repeats 10
```

`--source` 可以与测试脚本所在仓库不同：测试脚本在交付分支上，被测源码仍保持冻结。每种 HTTP 场景建立新 Gaia 实例、配置、数据库和目标进程，避免上一轮残留影响下一轮。

预跑使用 `--repeats 1`。可用 `--cases monitoring` 单独测试监控，或逗号分隔选取 HTTP 场景。不要并行启动多套脚本。

## 覆盖与判定

| 场景 | 判定 |
| --- | --- |
| monitoring | 内置路径/自定义路径、白名单外 exec、普通进程 high 与受监控服务 high/critical 权限变更告警；端口 bind/connect 被拒绝，退出后可重新 bind |
| monitor-control | 仓库原高频目标只挂探针，reload 成功且目标存活 |
| reload-original | 仓库原高频目标添加补丁后 reload，记录是否复现 SIGSEGV 与 success=false；**复现故障不等于功能通过** |
| missing-abi | reload 报告写入失败，目标持续返回 10 |
| compatible / cooperative | 添加后实际返回 99，删除后实际返回 10 |
| compatible-reload / cooperative-reload | 五字节首指令目标添加后 99、reload 成功且仍返回 99、删除后 10；与原高频目标分开记录 |
| skip | 指定 skip_call、返回值 0，删除后返回 10 |
| put | 添加后 99，修改后 77，删除后 10 |
| replace-cooperative / replace-compatible | replace_function 写入被拒绝，目标保持 10；另存 HTTP 状态与响应，区分配置接收和执行拒绝 |
| partial | 两个目标中一个缺 ABI，reload 报失败，但有效目标仍返回 99，证明非事务行为 |
| pie | 带符号的 PIE 目标可解析地址，添加后 99，删除后 10 |

`matched` 仅表示符合该场景预期。已知崩溃用例的 matched=true 表示故障被复现。自定义敏感路径未告警时保留 false，不把现有缺陷改成“预期通过”。

返回值来自新测试目标的实际调用日志，按每个阶段 150 ms 窗口采样，使用目标内部 CLOCK_MONOTONIC 调用完成时间。最初两批记录按 Python 收到输出的时间分窗，有管道延迟边界，详见补测报告；实际执行的脚本快照分别保留，不能用新版脚本解释旧数据。采样不能排除阶段间短暂混用。协作目标使用协议 ABI 和固定线程，不能代表不遵守协议的任意程序。原高频目标保留原样用于故障复现。

## core 与探针证据

每次套件只为第一轮原高频崩溃目标启用 core，设置该子进程的 coredump_filter 以保留被修改的代码页；其余目标不生成 core。GDB 只读取事后 core，不在线附加，避免与 Gaia 的 ptrace 冲突。

bpftool 的完整输出保存于 `before-reload-probes.json` 和 `after-reload-probes.json`。分析按目标可执行文件、偏移、程序 ID 和 Gaia 所持 FD 关联，不能把输出的 FD 持有者 PID 误当目标 PID。工具版本限制和失败也作为证据保存。

输出包含环境、二进制哈希、编译命令、HTTP 逐步记录、实际返回值、守护进程日志和逐轮结果。core、数据库及目标二进制保留在测试机，不纳入 Git；提交脱敏结果与必要摘录。`cleanup.json` 与监控后的 bind 对照用于核对收尾。

权限测试调用 `setuid(getuid())`，验证“调用告警”，不声称发生了成功提权；两个不同 comm 的专用进程用于区分普通进程和服务。敏感路径用合成文件，不读取真实口令或密钥。
