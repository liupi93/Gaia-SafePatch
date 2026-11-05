# 任务一：Gaia 原有功能复现

## 统一验收环境

| 项目 | 统一值 |
| --- | --- |
| 操作系统 | openEuler 24.03 LTS SP4 服务器版 |
| 架构 / 内核 | x86_64 / SP4 自带 6.6 系列内核 |
| 虚拟机 | 4 vCPU、8 GB 内存、60 GB 动态磁盘 |
| Rust / eBPF | `nightly-2025-10-01`（LLVM 21.1）/ Aya `a9945b0` |
| 构建工具 | LLVM、Clang、bpf-linker、GCC |
| 前端 | Node.js 22 LTS、pnpm 9.x |
| Git | 从 `origin/base` 建立 `dzz` |

WSL、Docker Desktop 或其他发行版只能预检查，不代替正式验收。安装前保存 VM 快照并保留未修改的 `base` 基线。

## 系统与构建

```bash
cat /etc/openEuler-release; uname -m; uname -r
test -r /sys/kernel/btf/vmlinux
grep -E 'CONFIG_(BPF|BPF_SYSCALL|DEBUG_INFO_BTF|UPROBE_EVENTS)=' /boot/config-"$(uname -r)"
sudo dnf install -y git curl gcc gcc-c++ make clang llvm lld elfutils-libelf-devel \
  openssl-devel pkgconf-pkg-config jq binutils
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
rustup toolchain install nightly-2025-10-01 --component rust-src clippy rustfmt
# bpf-linker 默认使用 LLVM 21，必须与该 nightly 生成的 LLVM IR 一致。
cargo +nightly-2025-10-01 install bpf-linker --version 0.9.15 --locked
# 安装 Node.js 22 LTS 后：
sudo npm install -g pnpm@9
git clone --branch base https://github.com/liupi93/Gaia-SafePatch.git
cd Gaia-SafePatch && git switch -c dzz
cargo build --release -p gaia-xdp
sha256sum target/release/gaia-xdp
```

openEuler SP4 默认安装可能仍使用 cgroup v1。若启动日志报告找不到 cgroup v2，执行
`grubby --update-kernel=ALL --args="systemd.unified_cgroup_hierarchy=1"` 并重启；随后用
`stat -fc %T /sys/fs/cgroup` 确认输出为 `cgroup2fs`。

## 启动、监控与 API

复制配置并保持示例热补丁目标禁用，然后启动：

```bash
cp gaia.toml gaia-task1.toml
sudo RUST_LOG=info ./target/release/gaia-xdp --config ./gaia-task1.toml \
  --web-listen 0.0.0.0:17890 2>&1 | tee task1-gaia.log
```

另开终端验证：

```bash
curl -fsS http://127.0.0.1:17890/api/v1/state | jq .
curl -fsS http://127.0.0.1:17890/api/v1/events | jq .
```

在受控终端触发配置覆盖的文件、进程与网络事件，在日志/API/WebUI 保存事件。服务名必须按 VM 实际 systemd 服务调整，不得伪造结果。

## 用户态热补丁验收

```bash
sudo THREADS=8 bash experiments/concurrency/run_experiment.sh | tee task1-hotpatch.log
```

通过条件：Gaia 和目标进程在补丁前后 PID 均不变；日志显示 uprobe/uretprobe 挂载、符号解析与 code patch applied；旧调用符合 `input+100`，新调用返回 `200`；事件 API 能看到目标函数事件。

| 检查项 | 实际结果 | 状态 |
| --- | --- | --- |
| openEuler SP4 / x86_64 / 6.6 | `24.03 LTS-SP4`，内核 `6.6.0-159.4.3.154.oe2403sp4.x86_64` | 通过 |
| BTF/BPF/uprobe | BTF 可读，`CONFIG_BPF`、`CONFIG_BPF_SYSCALL`、`CONFIG_DEBUG_INFO_BTF`、`CONFIG_UPROBE_EVENTS` 均为 `y` | 通过 |
| release 构建 | 构建成功，二进制 SHA-256 `8c604aa63f39893ef094326b894cfeebb2ed508ce96db14f3d6caf42b5f29cd5` | 通过 |
| Gaia API 与 eBPF 事件 | API HTTP 200；六项 feature 均为 `true`；已采集 file/process/network/hotpatch/privilege 计数 | 通过 |
| 用户态热补丁 | Gaia PID 在实验期间保持 `1848`；30 轮均把后续调用切换为返回 `200`，共 `240/240` 个请求观察到新语义 | 通过 |

实测时间为 2026-11-06，代码基线 `0c6879f`。环境、启动探针及 API 摘要见
`docs/evidence/openeuler-sp4/task1-runtime.txt`；任务二的 30 轮逐请求原始输出见
`experiments/concurrency/results/openeuler-sp4-30-rounds.log`。
