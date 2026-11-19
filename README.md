# Agent Coordinator

Agent Coordinator 是一个本地 Codex 插件。多个聊天可以共用同一个 Git 工作目录。编辑前先申请文件锁，有冲突就等待，改不同文件可以并行。

## 安装

从 [GitHub Releases](https://github.com/Terie412/agent-coordinator/releases/latest) 下载对应系统的压缩包，解压到固定目录。

| 系统 | 下载包 |
| --- | --- |
| Windows x64 | [下载 ZIP](https://github.com/Terie412/agent-coordinator/releases/latest/download/agent-coordinator-windows-x64.zip) |
| macOS Apple 芯片 | [下载 TAR.GZ](https://github.com/Terie412/agent-coordinator/releases/latest/download/agent-coordinator-macos-arm64.tar.gz) |
| macOS Intel 芯片 | [下载 TAR.GZ](https://github.com/Terie412/agent-coordinator/releases/latest/download/agent-coordinator-macos-x64.tar.gz) |
| Linux x64 | [下载 TAR.GZ](https://github.com/Terie412/agent-coordinator/releases/latest/download/agent-coordinator-linux-x64.tar.gz) |

在解压目录的终端运行下面两条命令。

```sh
codex plugin marketplace add .
codex plugin add agent-coordinator@agent-coordinator-local
```

打开 Codex 和目标 Git 项目，首次使用时在 **Review hooks**（CLI 用 `/hooks`）审查并信任 Agent Coordinator 的钩子。

如果用过 v0.1 的 `configure`，请移除项目配置里的旧 `mcp_servers.agent_coordinator` 和指向旧版可执行文件的钩子，保留其他设置。已有锁可继续使用。

## 启停与使用

在 Codex 插件列表（CLI 用 `/plugins`）开启或关闭 **Agent Coordinator**。锁工具和写入检查会从下一轮对话一起启停。停用前先结束正在写入的任务并释放锁。停用期间，已有锁记录仍会保留。

启用后，Codex 会在补丁编辑和常见 Git 写操作前检查锁。提交只带当前聊天负责的文件。脚本和外部编辑器也需要主动协调写入。

## 恢复遗留锁

用插件包 `plugins/agent-coordinator/bin/` 下的可执行文件运行 `agent-coordinator status <仓库路径>`。确认旧任务已停止且遗留改动已处理后，用 `agent-coordinator recover <仓库路径> <会话ID> <锁ID>` 释放对应的锁。
