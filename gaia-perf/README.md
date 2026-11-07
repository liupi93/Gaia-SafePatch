# Gaia 性能测试与 2026-11-07 实测

这套工具负责原版与并发一致性感知优化版的性能评估。正式采集核心只用 Python 3.11+ 标准库，Linux 直接读取 /proc。已在华为云 openEuler 24.03 LTS、x86_64、4 vCPU 环境完成一次 16 轮整机实验，全部轮次有效。结果、配置、环境锁、构建清单和原始 CSV 一并保存。

本分支基于优化版提交 be3dd66ecc1aa4479a490df8cfb8caefa9c2c836，新增文件集中在 gaia-perf/。实验使用的两个源码提交如下。

|版本|源码提交|
|---|---|
|base 原版|eec1660a4d6679fb3ce687b757a25578a6abd326|
|main 优化版|be3dd66ecc1aa4479a490df8cfb8caefa9c2c836|

## 查看结果

正式实测的入口是 [recorded/2026-11-07/README.md](recorded/2026-11-07/README.md)。其中包含每轮和对照统计、Markdown 报告、配置及版本证据。formal-results.tar.gz 保存 16 轮的请求、资源、补丁事件、时间线、逐轮摘要、日志、配置和两版插桩差异，SHA256SUMS 可用于校验。

此前的写入器微基准另存于 recorded/2026-11-07-micro/。该微基准使用 O0 C 目标和 debug 驱动，其统计范围与这里的整机实验各自独立。

## 实验分组

|组|版本|补丁|协作路径|
|---|---|---|---|
|G1|original|无|原版调用方式|
|G2|original|第 8 秒提交一次|原版调用方式|
|G3|optimized|无|门禁、活动计数及优化监测开启|
|G4|optimized|第 8 秒提交一次|与 G3 相同配置|

每组分别运行 4、8 线程，各重复两次，共 16 轮。默认每轮预热 3 秒，测量 20 秒，资源每秒采样。启动预算 8 秒，收尾预算 4 秒，其中在途请求收尾最多 2 秒。套件预算 600 秒并预留 40 秒，余额不足时整轮记录为 budget_skipped。相同场景、线程和重复的两版相邻，先后顺序按种子随机决定。

两版使用同一个受控 C 程序 formal/workload.c。请求执行相同的确定性计算，再调用 target_value。G3/G4 开启该程序的协作门禁和活动计数，G1/G2 使用原版调用方式。请求在进入准入等待前计时，每线程一次处理一个请求，属于固定并发封闭式负载。结论适用于这个受控负载。

两版共同使用 override_return=99，初始 target_value 返回 10。该动作与当前 main 支持的 cooperative 模式匹配。formal/instrument.py 在独立工作树中对两版 writer 的同一阶段加入 CLOCK_MONOTONIC 起止时间，并在优化版记录门禁关闭和重开时刻。插桩差异和哈希随结果保存。目标首次观察到返回值 99 的时刻作为独立证据保存。

## 在已有服务器复现

需要已有 Git、Python 3.11+、C 编译器、Rust/BPF 工具链、pnpm 及项目冻结依赖缓存。运行身份为 root，架构为 x86_64。脚本采用仓库 rust-toolchain.toml 所选的已安装 nightly，设置 RUSTUP_AUTO_INSTALL=0、CARGO_NET_OFFLINE=true，并以 --release --locked 构建两版。本次实际使用 rustc 1.101.0-nightly（1c3a65069 2026-11-02），完整标识见 suite.json。

```bash
git fetch origin base main perf/gaia-benchmark-20261107
git switch perf/gaia-benchmark-20261107
bash gaia-perf/formal/reproduce.sh
```

脚本创建独立工作树和输出目录，编译两份 release 和相同业务程序，再生成构建清单和正式配置。它先做四组、4 线程、每组一次的短测，测量窗口为 12 秒，以覆盖第 8 秒补丁提交。短测通过后自动运行默认 16 轮。

collect.sh 在没有 cgroup2 挂载时挂载 /run/gaia-cgroup2。runner.py 预检查 root、BTF、可用 cgroup2、端口、磁盘、环境锁及构建清单。Gaia 只监听 127.0.0.1:17890，每轮使用独立业务进程、Gaia 进程、配置和数据库。收尾按自建进程句柄执行，保留失败和中断结果。

通用接入使用 config.json 中的程序路径、启动参数数组、目标符号、构建清单和环境锁。以下命令在配置完成后执行。

```bash
cd gaia-perf
bash run.sh --config config.json --variant original --mode steady --dry-run
bash run.sh --config config.json --preflight
bash run.sh --config config.json --variant both --mode both
python3 analyze.py --input results/实际套件目录
```

可选 --plots 在已有 Matplotlib 的分析环境离线运行。编译、准备和绘图安排在正式测量之外。

## 接入与统计口径

业务协议为 stdin-lines-v1。启动后原子写入 GAIA_PERF_STATUS_FILE，包含 run_id、pid、clock=CLOCK_MONOTONIC、process_model=single-process-multithread、phase=ready 和 ack_ns。接收 WARMUP 后确认 warmup。接收 MEASURE run_id start_ns end_ns 后，在 start_ns 前确认相同窗口。到 end_ns 自动停止产生新请求。STOP 排空并批量导出请求 CSV，再确认 stopped 和正式到达请求的 generated_count，等待运行器清理。

请求 CSV 列为 run_id,request_id,worker_id,phase,arrival_ns,start_ns,end_ns,status。arrival_ns 在等待准入之前，end_ns 留空表示未完成。状态为 ok/error/timeout/cancelled。运行器给标准化文件补充 case_id。仅完整计数的业务可使用 counts-v1，此时延迟分位数保持缺失。

补丁事件 CSV 列为 run_id,attempt_id,event,mono_ns,outcome，attempt_id=patch-1。运行器提供 request_sent/http_done，服务端适配器提供 request_received、apply_begin/apply_end，可选 admission_closed/admission_resumed。实际失败保留失败结果。新实现证明包含 run_id、pid、attempt_id、observed_ns、new_implementation=true 和 evidence。

优化版启用证明来自本轮目标的实际 uprobe 事件、/api/v1/hotpatch-observation 返回的 TGID 记录、cooperative 配置和业务 ABI 确认。当前 main 使用 TGID 索引 legacy PID 审计过滤器，其 uprobe-entry:pid-filtered 标签可作为入口观测证据，随后仍要核对优化接口及协作状态。

- CPU 读取 /proc，采用单核 100% 口径，多线程进程可超过 100%。RSS 为 MiB，最大值标为采样峰值。整机单列 steal，guest 不重复计数。
- 测量窗口为 [start_ns,end_ns)。完成 QPS 包含窗口内全部成功完成，含预热请求跨入窗口的完成。延迟仅针对正式窗口生成请求，以 end-arrival 计算，包含准入等待和收尾。
- 未完成请求单列，分位数使用已结束请求并显示样本量。P95/P99 使用 nearest-rank。组摘要先计算每轮分位数，再给出均值和范围。
- 补丁 HTTP 往返、真实 writer 阶段、新实现观察时刻分别保存。HTTP 返回配置不能独立证明补丁应用。任一显式应用阶段失败都会使轮次失效。
- 两次重复用于快速描述性比较。吞吐下降率=(基准QPS−候选QPS)/基准QPS×100%。CPU 增量单位为百分点，RSS 增量单位为 MiB。缺值及零分母留空。

本次脚本完成 40 项本地回归测试，并从正式原始数据独立离线重算。runs.csv、comparison.csv、paired.csv 与服务器结果一致。

## 本地脚本自测

```bash
python3 -m unittest discover -s gaia-perf/tests -v
python3 gaia-perf/tests/make_synthetic_config.py --output gaia-perf/synthetic.local.json
python3 gaia-perf/runner.py --config gaia-perf/synthetic.local.json --synthetic
```

synthetic 配置需要显式 --synthetic，所有这类轮次的 valid_for_comparison=false。实际整机数据位于 recorded/2026-11-07/，其 synthetic=false。
