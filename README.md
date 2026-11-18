# Agent Coordinator

让多个本地 Codex 聊天共用同一个 Git 工作目录。会话改文件前先申请锁。碰到别的会话占用就等待，改不同文件的会话可以并行。

## 开始使用

需要 Git、支持 MCP 和钩子的本地 Codex，以及 Rust 1.88 或更新版本。Windows 编译还需要 Visual Studio C++ Build Tools。

```powershell
git clone https://github.com/Terie412/agent-coordinator.git
cd agent-coordinator
cargo build --release --locked
$coordinator = (Resolve-Path '.\target\release\agent-coordinator.exe').Path
& $coordinator configure 'C:\path\to\your-repo'
```

在 Codex 中打开目标仓库，审查并信任项目钩子（桌面版 **Review hooks**，CLI 用 `/hooks`）。然后让多个本地聊天使用同一个仓库目录。

如果项目已有 `.codex/config.toml`，把命令里的 `configure` 换成 `config`，将打印出的配置合并进去。

## 日常使用

- **编辑文件**时，让 Codex 一次申请本次要改的所有文件。拿到锁后重新读取，提交或交接后释放。
- **提交或切换分支**前，让 Codex 申请对应的 Git 锁或工作目录锁。提交只带本会话负责的文件。
- **查看占用**用 `& $coordinator status '仓库路径'`。会话中断后，确认写入已停止、遗留改动已处理，再用 `& $coordinator recover '仓库路径' '会话ID' 锁ID` 释放。

钩子会检查 `apply_patch` 和常见 Git 写操作。脚本和其他编辑器写文件前也需要主动协调。
