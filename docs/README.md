# Gaia-SafePatch 文档导航

这组文档面向第一次接触多线程和热补丁的读者。这里的“优化”主要是**提高热补丁的安全性**，会增加协调和等待开销，不代表运行速度更快。

建议按下面的顺序阅读：

0. [项目说明](project-guide.md)：项目全貌，包括本组工作、安装运行、测试与性能结果、已知问题和分工。**第一次阅读从这里开始。**
1. [从零理解安全热补丁](safepatch-explained.md)：为什么会出问题，门禁、排空和暂停分别解决什么问题，以及代码如何实现。
2. [运行测试与解读结果](safepatch-testing.md)：一键测试、日志含义、原版风险对照、性能数据和排错方法。
3. [功能测试与边界分析报告](functional-test-report.md)：在 `be3dd66` 上的首轮与服务器补测结果、reload core 分析、自定义敏感路径漏报和剩余验收项。证据在 [evidence/be3dd66-functional](evidence/be3dd66-functional/README.md)，复现入口见 [功能测试脚本](../tests/functional/README.md)。

如果只想找到代码，从这些文件入手：

| 文件 | 用途 |
|---|---|
| [safepatch.rs](../gaia-xdp/src/safepatch.rs) | 安全协调器，执行暂停、排空、写入和恢复 |
| [main.rs](../gaia-xdp/src/main.rs) | 符号地址解析、生成补丁、保存与恢复原始字节 |
| [web.rs](../gaia-xdp/src/web.rs) | reload 接口的成功判定、观测接口 |
| [eBPF main.rs](../gaia-xdp-ebpf/src/main.rs) | 函数进入、返回与线程状态观测 |
| [target.h](../tests/safepatch/target.h) | 协作式 C 目标程序，含入口门禁示例 |
| [driver.rs](../tests/safepatch/driver.rs) | 调用真实运行时的测试驱动 |

当前安全运行时针对 **Linux x86_64、固定线程集、遵守协作协议的目标程序**。它不保证可以安全修改任意已经运行的第三方程序。完整适用条件见原理文档。
