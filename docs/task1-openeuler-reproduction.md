# 任务一：Gaia 原有功能复现

## 统一验收环境

| 项目 | 统一值 |
| --- | --- |
| 操作系统 | openEuler 24.03 LTS SP4 服务器版 |
| 架构 / 内核 | x86_64 / SP4 自带 6.6 系列内核 |
| 虚拟机 | 4 vCPU、8 GB 内存、60 GB 动态磁盘 |
| Rust / eBPF | 仓库锁定 nightly / Aya 原依赖 |
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
rustup toolchain install nightly --component rust-src clippy rustfmt
cargo +nightly install bpf-linker --locked
# 安装 Node.js 22 LTS 后：
sudo npm install -g pnpm@9
git clone --branch base https://github.com/liupi93/Gaia-SafePatch.git
cd Gaia-SafePatch && git switch -c dzz
cargo build --release -p gaia-xdp
sha256sum target/release/gaia-xdp
```

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
| openEuler SP4 / x86_64 / 6.6 | 待 VM 实测 | 待验证 |
| BTF/BPF/uprobe | 待 VM 实测 | 待验证 |
| release 构建 | 待 VM 实测 | 待验证 |
| Gaia API 与 eBPF 事件 | 待 VM 实测 | 待验证 |
| 用户态热补丁 | 待 VM 实测 | 待验证 |

只有统一 openEuler VM 中真实得到的结果才能改为“通过”。
