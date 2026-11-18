# Agent Coordinator

让 Codex、DeepSeek Harness、Claude Code、Pi、OpenCode、WorkBuddy 和 Qoder 等 Agent 共用同一个本地 Git 工作目录。改文件前先申请锁，有冲突就等待，改不同文件可以并行。

## 安装

需要 Git 和 Rust 1.88+。下方是 Windows 示例，编译需要 Visual Studio C++ Build Tools。

```powershell
git clone https://github.com/Terie412/agent-coordinator.git
cd agent-coordinator
cargo build --release --locked
$coordinator = (Resolve-Path '.\target\release\agent-coordinator.exe').Path
$repo = 'C:\path\to\your-repo'
```

## 接入 Agent

**Codex** 运行 `& $coordinator configure $repo`，打开目标仓库并审查钩子（桌面版 **Review hooks**，CLI 用 `/hooks`）。已有 `.codex/config.toml` 时，用 `& $coordinator config $repo` 打印配置后合并。

**其他 Agent** 用下表生成本地 MCP 配置，服务器名称为 `agent_coordinator`。各 Agent 要指向同一个 `$repo`。

| Agent | 配置命令 | 配置位置 |
| --- | --- | --- |
| Claude Code、Qoder | `& $coordinator config $repo mcp` | 合并到项目的 `.mcp.json`。[Claude Code](https://code.claude.com/docs/en/mcp) · [Qoder](https://docs.qoder.com/cli/mcp-reference) |
| Pi | `& $coordinator config $repo mcp` | 支持内置 MCP 的版本合并到 `.pi/mcp.json`，旧版用下方的终端方式。[Pi MCP](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/mcp.md) |
| OpenCode | `& $coordinator config $repo opencode` | 合并到 `opencode.json`。v1 使用 `opencode-v1`。[OpenCode MCP](https://opencode.ai/docs/mcp-servers/) |
| DeepSeek Harness | `& $coordinator config $repo deepseek` | 在 Harness 的 MCP 设置中添加本地服务器，填入生成的命令和参数。[DeepSeek MCP](https://github.com/deepseek-ai/deepseek-harness/blob/master/packages/mcp/mcp-manager/README.md) |
| WorkBuddy | `& $coordinator config $repo mcp` | 添加本地 stdio MCP，使用生成配置中的命令和参数。[WorkBuddy MCP](https://open.workbuddy.cn/docs/connector) |

告诉 Agent 每次修改前先用 `acquire` 一次申请本次涉及的所有文件。拿到锁后重新读文件，提交或交接后用 `release` 释放。提交只带本会话负责的文件，并申请 `git` 锁。切分支或合并前申请 `checkout` 锁。

Codex 钩子自动识别会话，并检查 `apply_patch` 和常见 Git 写操作。其他客户端由每个聊天调用一次 `new_session`，之后用同一个 `owner` 申请和释放。脚本和编辑器也按这条规则协调写入。

### 终端接入

能运行本地命令的 Agent 也可以直接调用 CLI。每个聊天保留自己的 `$owner`，只有申请成功才编辑，完成后用实际的锁 ID 释放。

```powershell
$owner = (& $coordinator new-session $repo | ConvertFrom-Json).owner
& $coordinator acquire $repo $owner --wait 5 -- 'src/main.rs'
& $coordinator release $repo $owner 123
```

用 `& $coordinator status $repo` 查看占用。如果原会话已停止写入且遗留改动已处理，可用 `& $coordinator recover $repo '会话ID' 123` 释放遗留锁。
