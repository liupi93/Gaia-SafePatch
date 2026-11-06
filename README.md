<div align="center">

# 🛡️ openeuler-service-monitor

**基于 Rust + eBPF 的 OpenEuler 系统关键服务监控与主动防御工具**

[![Rust](https://img.shields.io/badge/language-Rust-orange?logo=rust)](https://www.rust-lang.org/)
[![eBPF](https://img.shields.io/badge/technology-eBPF-blue)](https://ebpf.io/)
[![Platform](https://img.shields.io/badge/platform-OpenEuler%205.10%2B-red)](https://openeuler.org/)
[![Arch](https://img.shields.io/badge/arch-x86__64%20%7C%20ARM64-green)]()
[![License](https://img.shields.io/badge/license-MIT-lightgrey)](LICENSE)
[![Build Status](https://img.shields.io/badge/build-passing-brightgreen)]()

[简介](#-项目简介) · [特性](#-核心特性) · [快速开始](#-快速开始) · [架构](#-系统架构) · [模块说明](#-模块说明) · [配置](#-配置参考) · [开发指南](#-开发指南) · [路线图](#-开发路线图) · [贡献](#-贡献指南) · [许可证](#-许可证)

</div>

---

## Gaia-SafePatch 入门文档

安全热补丁的实现原理、协作协议、测试命令和实测结果解读，见 [docs 文档导航](docs/README.md)。

## 📖 项目简介

`openeuler-service-monitor` 是一款专为 **OpenEuler** 操作系统量身定制的轻量级、高安全、低开销的系统关键服务监控与主动防御工具。

本项目以 **Rust 语言**和 **eBPF 技术**为核心，通过 [Aya 框架](https://aya-rs.dev/) 实现内核态与用户态的全栈 Rust 开发，对运行在 OpenEuler 上的关键服务（如 systemd 托管服务、数据库、Web 服务器等）进行细粒度的行为监控、异常检测与运行时热补丁修复，在不修改内核源码、不重启系统的前提下，实现"观测即防御"的一体化安全能力。

### 🎯 解决的核心问题

| 传统监控痛点 | 本工具解决方案 |
|---|---|
| `strace`/`ptrace` 性能开销极高（慢 10x+） | eBPF 内核态直接过滤，近乎零开销 |
| 通用工具无法深度适配 OpenEuler 特性 | 深度集成 systemd、BTF/CO-RE、BPF LSM |
| 监控工具自身存在内存安全风险 | 全栈 Rust 实现，编译期保证内存安全 |
| 漏洞修复必须重启服务/系统 | 基于 eBPF 的运行时热补丁，零停机修复 |
| 部署依赖复杂（Python、LLVM 等） | 单一静态二进制，零外部依赖 |

---

## ✨ 核心特性

### 🔍 多维度服务行为监控
- **文件系统访问监控**：实时捕获对 `/etc/shadow`、SSL 证书、配置文件等敏感路径的读写操作
- **进程生命周期监控**：追踪服务进程的创建（`execve`）、退出、特权提升（`setuid`/`setgid`）
- **网络活动监控**：检测非预期的外连行为、反向 Shell、异常 C2 通信
- **系统调用异常检测**：基于行为基线的系统调用序列分析，识别偏离正常模式的异常执行

### 🔥 运行时热补丁（无需重启）
- **内核态函数热补丁**：通过 `kprobe` + `bpf_override_return` 对高危内核函数进行参数校验和返回值干预
- **用户态函数热补丁**：通过 `uprobe` 对 Nginx、OpenSSH、Redis 等关键服务的高危函数进行运行时缓解
- **补丁热管理**：支持补丁的动态启用/禁用、版本管理与一键回滚

### ⚡ 高性能低开销
- eBPF JIT 编译执行，内核态直接过滤，用户态仅处理关键事件
- Per-CPU 数据结构，消除多核竞争
- BPF Ring Buffer 高吞吐传输，支持背压反馈节流
- 异步 Tokio 运行时，非阻塞批量事件消费

### 🏗️ OpenEuler 深度适配
- 基于 **BPF CO-RE**（一次编译，到处运行），无需内核头文件
- 深度集成 **systemd DBus** 接口，自动发现服务进程，无需手动配置 PID
- 支持 **BPF LSM** 强制访问控制策略动态注入
- 已验证 **x86_64** 与 **鲲鹏/飞腾 ARM64** 双架构兼容性

### 🦀 全栈 Rust 实现
- 内核态 eBPF 程序与用户态控制程序均使用 Rust 编写
- 跨层共享数据结构定义，类型安全，无 ABI 不一致风险
- 编译为单一静态链接二进制文件，部署极简

---

## 🚀 快速开始

### 环境要求

| 依赖项 | 最低版本 | 说明 |
|---|---|---|
| OpenEuler | 22.03 LTS (内核 5.10+) | 需开启 `CONFIG_DEBUG_INFO_BTF=y` |
| Rust 工具链 | 1.75.0 (stable) | 含 `rustup` |
| Rust nightly | nightly-2024-01-01+ | 用于编译 eBPF 内核程序 |
| pnpm | 9+ | 用于构建 `gaia-webui`，已集成到 `cargo build` |
| bpftool | 5.10+ | 用于 BTF 对象生成（可选） |
| Linux 内核头文件 | 与运行内核一致 | 仅开发环境需要 |

> **注意**：运行时无需 Python、LLVM、内核头文件等额外依赖。

### 安装 Rust 工具链

```bash
# 安装 rustup
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# 添加 nightly 工具链（用于 eBPF 内核程序编译）
rustup toolchain install nightly

# 安装 cargo-generate（用于项目模板，可选）
cargo install cargo-generate

# 安装 bpf-linker（eBPF 程序链接器）
cargo install bpf-linker

# 安装 pnpm（前端构建工具，cargo build 会自动调用）
npm install -g pnpm

```

## 从源码构建

```bash
# 在项目根目录执行；会自动触发 gaia-webui 的 pnpm build
cargo build
```

> 构建前请确认系统中已安装 `pnpm`，并且 `gaia-webui` 依赖已经准备好。

## 运行

```
sudo RUST_LOG=info ./target/debug/gaia-xdp --config gaia.toml
```

前端在gaia-webui/下。

## 系统架构

┌─────────────────────────────────────────────────────────────────────┐
│                        用户交互层 (CLI / Dashboard)                   │
│              monitor CLI  │  Terminal TUI  │  HTTP API               │
└──────────────────────────┬──────────────────────────────────────────┘
                           │
┌──────────────────────────▼──────────────────────────────────────────┐
│                        用户态处理层 (Rust / Tokio)                    │
│  ┌─────────────┐  ┌──────────────┐  ┌────────────┐  ┌───────────┐  │
│  │ 事件解析器   │  │  异常检测引擎  │  │ 热补丁管理器 │  │ 告警模块  │  │
│  │ (Deserialize)│  │(Rule/Baseline)│  │(Patch Mgr) │  │(Alert Mgr)│  │
│  └──────┬──────┘  └──────┬───────┘  └─────┬──────┘  └─────┬─────┘  │
│         └────────────────┴────────────────┴───────────────┘        │
│                                    │                                 │
│  ┌─────────────────────────────────▼──────────────────────────────┐ │
│  │              事件传输层 (BPF Ring Buffer / Perf Buffer)          │ │
│  └─────────────────────────────────┬──────────────────────────────┘ │
└─────────────────────────────────────┼───────────────────────────────┘
                                      │  (内核态 ↔ 用户态边界)
┌─────────────────────────────────────▼───────────────────────────────┐
│                      内核态 eBPF 探针层 (Rust / Aya)                  │
│                                                                      │
│  ┌──────────────┐  ┌──────────────┐  ┌────────────┐  ┌───────────┐ │
│  │ 文件访问探针  │  │ 进程生命周期  │  │ 网络活动   │  │ 热补丁    │ │
│  │ (tracepoint/ │  │   探针       │  │   探针     │  │  探针     │ │
│  │  kprobe)     │  │ (execve/     │  │(connect/   │  │(kprobe/   │ │
│  │              │  │  setuid)     │  │ bind)      │  │ uprobe)   │ │
│  └──────────────┘  └──────────────┘  └────────────┘  └───────────┘ │
│                                                                      │
│  ┌──────────────────────────────────────────────────────────────┐   │
│  │                BPF LSM 强制访问控制层                          │   │
│  └──────────────────────────────────────────────────────────────┘   │
└──────────────────────────────────────────────────────────────────────┘
                                      │
┌─────────────────────────────────────▼───────────────────────────────┐
│                     OpenEuler 内核 (5.10+)                           │
│         BTF/CO-RE  │  BPF LSM  │  systemd DBus  │  ARM64 支持        │
└──────────────────────────────────────────────────────────────────────┘

