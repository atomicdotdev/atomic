# ACP Adapter、Atomic Agent Package 和 noname GUI 的关系

这份文档解释 noname/Sherpa 2.0 里几个容易混淆的概念：

- ACP adapter 是什么
- `atomic-agent` 是什么
- `atomic-codex` / `atomic-claude` 是什么
- noname GUI 为什么需要本机安装这些东西
- Ask 时到底是谁调用谁

## 一句话结论

当前 noname 的 Ask 流程不是直接运行 `atomic-agent`。

当前流程是：

```text
noname GUI
  -> noname Rust backend
  -> spawn 本机 ACP adapter，比如 codex-acp / claude-acp
  -> adapter 驱动真实 agent，比如 Codex / Claude
  -> agent 读代码、回答、改文件
  -> noname 外层用 Atomic lifecycle/hooks 记录 change
```

所以本地要让 Ask 可用，通常需要两类东西：

```text
1. ACP adapter binary
   例如 codex-acp、claude-acp

2. Atomic agent package
   例如 atomic-codex、atomic-claude
```

## ACP adapter 是什么

ACP 是 Agent Client Protocol。它的作用是让 GUI 用同一种协议和不同 agent 对话。

不同 agent 原本有不同的启动方式、输入格式、流式输出格式、tool-call 格式。ACP adapter 把这些差异统一起来。

可以理解成：

```text
noname speaks ACP

codex-acp       translates ACP <-> Codex
claude-acp      translates ACP <-> Claude
opencode adapter translates ACP <-> OpenCode
```

ACP adapter 负责：

- 启动真实 agent runtime
- 接收 noname 发来的 prompt
- 把 agent 输出转成统一的 ACP event
- 把 tool call / permission request / final response 流式返回给 noname

ACP adapter 不负责 Atomic 记录。

Atomic 记录由 noname 外层调用：

```text
atomic agent lifecycle begin
atomic agent hooks sherpa session-start
atomic agent hooks sherpa turn-start
atomic agent hooks sherpa turn-end --json
atomic agent hooks sherpa session-end
atomic agent lifecycle end
```

## `atomic-agent` 是什么

仓库：`atomicdotdev/atomic-agents`

里面有两个相关 crate：

```text
atomic-agent-harness
atomic-agent
```

### `atomic-agent-harness`

这是今天 noname 真正在用的部分。

noname 通过它把 registry id 映射成本机可执行命令：

```text
codex-acp  -> command: codex-acp
claude-acp -> command: claude-acp
opencode   -> command: opencode
```

代码路径大致是：

```text
noname/src-tauri/src/acp.rs
  -> atomic_agent_harness::spawn::agent_for_registry_id(...)
```

也就是说，如果 Ask 选择 codex，noname 后端会找 PATH 里的：

```bash
codex-acp
```

如果 Ask 选择 claude，noname 后端会找 PATH 里的：

```bash
claude-acp
```

### `atomic-agent`

这个名字容易误导。它看起来像“通用 Atomic agent runtime”，但当前还不是 noname Ask 使用的 runtime。

目前 `atomic-agent` binary 的代码仍然是：

```text
load package
print package info
TODO: Start ACP agent server over stdio
```

所以现在 noname 不是运行：

```bash
atomic-agent --package atomic-codex
```

而是运行真实的 ACP adapter：

```bash
codex-acp
claude-acp
```

长期如果 `atomic-agent` 实现了 ACP server，它可以成为更统一的 runtime。但这不是当前实现。

## `atomic-codex` / `atomic-claude` 是什么

仓库：

- `atomicdotdev/atomic-codex`
- `atomicdotdev/atomic-claude`

它们是 Atomic agent package，不是 ACP adapter。

它们的作用是让对应 agent “Atomic-aware”：

- 提供 agent prompt
- 提供 Atomic skills
- 提供 hook manifest
- 安装/注册 Codex 或 Claude 的 Atomic hooks

例如：

```text
atomic-codex
  -> AGENTS.md
  -> skills/
  -> hooks/codex.atomic-hooks.json
  -> install.sh installs into ~/.codex

atomic-claude
  -> CLAUDE.md
  -> agents/
  -> skills/
  -> hooks/claude-code.atomic-hooks.json
  -> install.sh installs into ~/.claude
```

它们不等于 `codex-acp` / `claude-acp`。

## 当前 GUI 为什么显示 38 个 agent，但 Ask 是 0

Settings 里的 `All (38)` 来自 ACP 公共 registry。

这只是说明 registry 知道这些 agent，例如：

```text
codex
claude
opencode
gemini
cline
```

但是 Ask 只显示本机已经安装 Atomic integration package 的 agent。

当前 noname 默认检查目录：

```text
~/Projects/agents
```

它会看有没有：

```text
~/Projects/agents/atomic-codex
~/Projects/agents/atomic-claude
~/Projects/agents/atomic-opencode
```

所以：

```text
Settings: All (38)
```

表示 ACP registry 有 38 个 agent。

```text
Settings: Atomic (0)
Ask: No ACP agents found
```

表示本机默认 agents 目录里没有可用的 Atomic package。

## 本机要让 Codex 出现在 Ask，需要什么

需要两件事：

```text
1. codex-acp 在 PATH 上
2. ~/Projects/agents/atomic-codex 存在
```

示意：

```bash
command -v codex-acp
test -d ~/Projects/agents/atomic-codex
```

`codex-acp` 是 noname 要 spawn 的进程。

`atomic-codex` 是 GUI 用来判断 codex 是否有 Atomic integration 的 package，同时它安装 Codex 的 Atomic prompt、skills、hooks。

## 本机要让 Claude 出现在 Ask，需要什么

同样需要两件事：

```text
1. claude-acp 在 PATH 上
2. ~/Projects/agents/atomic-claude 存在
```

示意：

```bash
command -v claude-acp
test -d ~/Projects/agents/atomic-claude
```

注意：`@agentclientprotocol/claude-agent-acp` 这个 npm package 当前暴露的 binary 名可能是：

```text
claude-agent-acp
```

但 noname harness 当前找的是：

```text
claude-acp
```

所以本地测试时可能需要一个兼容 symlink：

```bash
ln -s /opt/homebrew/bin/claude-agent-acp /opt/homebrew/bin/claude-acp
```

长期应该修 harness 映射，让它使用真实 binary 名，避免用户手动建 symlink。

## Ask 时完整调用链

以 Codex 为例：

```text
用户在 noname GUI 点 Ask
  |
  v
noname frontend 调 Tauri command: acp_ask
  |
  v
noname Rust backend 生成 session_id
  |
  v
atomic agent lifecycle begin --owner sherpa --session <session_id> --json
  |
  v
atomic agent hooks sherpa session-start
  |
  v
atomic agent hooks sherpa turn-start
  |
  v
noname spawn codex-acp
  |
  v
codex-acp 驱动 Codex
  |
  v
Codex 回答或改文件
  |
  v
ACP events 流回 noname
  |
  v
atomic agent hooks sherpa turn-end --json
  |
  v
Atomic 返回 recorded/change_hash/view/files
  |
  v
noname 保存 run sidecar 并更新 GUI
  |
  v
atomic agent hooks sherpa session-end
  |
  v
atomic agent lifecycle end
```

关键点：

```text
codex-acp / claude-acp 负责执行 agent
noname/Sherpa 负责外层 lifecycle
Atomic hooks/lifecycle 负责记录 change
```

## 为什么需要 managed lifecycle

如果用户直接在终端运行 Codex：

```text
Codex owns lifecycle
Codex hooks record the turn
```

这没问题。

但如果 noname/Sherpa 启动 codex-acp：

```text
Sherpa owns lifecycle
codex-acp/Codex is inner executor
```

这时如果 Codex 自己的 Atomic hooks 也记录，就会发生 double orchestration：

```text
inner Codex hook records the change first
outer Sherpa turn-end sees nothing
```

所以我们加了 managed lifecycle：

```text
atomic agent lifecycle begin --owner sherpa ...
```

在这个 lifecycle 期间：

- owner 是 `sherpa`，Sherpa hook 正常运行
- inner agent 例如 `codex` / `claude-code` 的 hook no-op
- 最终 change 由 Sherpa 的 `turn-end --json` 记录并返回给 GUI

这样 GUI 才能拿到：

```json
{
  "recorded": true,
  "change_hash": "...",
  "view": "...",
  "files": ["..."]
}
```

## 当前本机测试需要的安装状态

为了让 noname GUI 的 Ask 能显示 Codex 和 Claude：

```text
~/Projects/agents/atomic-codex
~/Projects/agents/atomic-claude
```

并且 PATH 上有：

```text
codex-acp
claude-acp
```

安装 package 后还需要运行它们的 install script：

```bash
cd ~/Projects/agents/atomic-codex
./install.sh

cd ~/Projects/agents/atomic-claude
./install.sh
```

这些脚本会安装 hooks/skills 到：

```text
~/.codex
~/.claude
```

## 产品上的改进方向

当前 GUI 依赖用户手动准备：

```text
ACP adapter binary
Atomic package directory
package install script
```

这对开发测试可以接受，但不是最终产品体验。

更好的产品方向是：

- Settings 里显示 “missing adapter” 和 “missing package” 的具体原因
- 提供一键安装 / 修复按钮
- harness 使用真实 binary 名，例如 `claude-agent-acp`
- 长期让 `atomic-agent` 成为统一 ACP runtime，减少用户需要安装的分散组件

