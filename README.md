# Edge-Cloud Hybrid Microkernel AI Gateway — Stage 1 + Stage 2 (Rust + Tauri v2)

本仓库已完整落地**阶段一（核心数据面内核）**与**阶段二（Tauri v2 系统托盘 + 一键原子接管 CLI 配置 + Append-Only 本地 SQLite 轨迹回放与多模型 Fork & Diff）**。

## 仓库结构

```text
edge-gateway/
├── Cargo.toml                           # Rust Workspace 根配置（含 gateway-core / daemon / Tauri）
├── config.example.toml                  # 上游渠道池、Lock/Stage-Aware 路由与四级熔断配置
├── docs/
│   ├── STAGE1_RFC.md                    # 阶段一核心内核设计文档
│   └── STAGE2_RFC.md                    # 阶段二 Tauri 桌面端、原子接管与轨迹分叉设计文档
├── crates/
│   ├── gateway-core/                    # 纯 Rust 微内核库（内嵌进 Tauri 与 Daemon）
│   │   └── src/
│   │       ├── ir.rs                    # Lossless Canonical IR（cache_control / thinking / strict tool）
│   │       ├── codec/{anthropic,openai}.rs
│   │       ├── sse_fsm.rs               # 零缓冲 SSE 状态机 + Drop 驱动 Cancel Propagation
│   │       ├── router.rs                # 前缀缓存亲和路由 + P2C 并发/TTFT 调度
│   │       ├── error_gov.rs             # 四级 429/401/400 错误治理引擎
│   │       ├── trajectory.rs            # SQLite WAL 仅追加轨迹引擎（Session → Turn → Step + Fork 树）
│   │       ├── cli_takeover.rs          # Claude Code / Codex / Gemini CLI 原子配置接管与无损回滚
│   │       └── engine.rs                # 统一调度、热切路由模式、Fork & Diff 执行
│   └── gateway-daemon/                  # 独立本地守护进程（含 /api/sessions 与 /api/steps/:id/fork）
├── apps/desktop/                        # Tauri v2 系统托盘 + React/Vite/TS 控制台
│   ├── src-tauri/                       # 直接嵌入 gateway-core，零 IPC 开销
│   └── src/App.tsx                      # 渠道监控 / 一键 CLI 接管 / 轨迹回放与 Fork Diff Studio
└── tests/
    ├── mock_upstream.py                 # 本地三渠道 Mock 上游
    └── conformance_suite.py             # 阶段一 + 阶段二自动化契约测试（含 Fork & Diff 验证）
```

## 快速启动

### 1. 运行 Rust 单元测试（含 SQLite 轨迹与原子配置接管测试）

```bash
cargo test --workspace
```

### 2. 启动 Tauri v2 系统托盘与控制台（推荐）

```bash
cd apps/desktop
npm install
npm run tauri dev
```

启动后自动拉起内嵌代理 `127.0.0.1:8787` 与系统托盘菜单，支持：
* 托盘菜单热切 `Lock` / `Stage-Aware` 路由模式（无需重启终端）；
* **CLI Takeover** 页签一键原子接管 Claude Code、Codex CLI、Gemini CLI 配置，并自动生成 `.edge-gateway.bak` 备份以便一键还原；
* **Trajectory Fork & Diff** 页签按会话查看每一步的 KV Cache 命中率与 TTFT，并从任意历史步骤一键 Fork 到其他模型重跑对比。

### 3. 命令行模式与自动化契约测试

```bash
# 终端 1：启动 3 个本地 Mock 上游
python3 tests/mock_upstream.py

# 终端 2：启动独立守护进程（自动落库至 .edge-gateway/trajectory.db）
cargo run -p gateway-daemon -- config.example.toml

# 终端 3：运行阶段一 + 阶段二全量契约测试
python3 tests/conformance_suite.py
```
