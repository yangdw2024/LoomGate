# 阶段二工程设计文档（Stage 2 RFC）：Tauri v2 端侧控制面、原子配置接管与本地轨迹 Fork & Diff

## 1. 阶段二核心演进：同进程嵌入，零 IPC 开销

阶段二不将 Tauri 当作“套壳前端去调外部子进程”，而是直接将 `crates/gateway-core` 编译进 `apps/desktop/src-tauri`：

| 组件 | 运行方式 | 优势 |
|---|---|---|
| 数据面代理 (`127.0.0.1:8787`) | 在 Tauri `setup` 阶段通过 `tauri::async_runtime::spawn` 拉起 Axum 服务 | 与桌面端共享同一个 `Arc<GatewayEngine>` 与 Tokio 运行时，内存占用低且无跨进程通信延迟 |
| 系统托盘 (`TrayIconBuilder`) | 托盘菜单直接调用 `engine.set_route_mode(...)`（原子变量 `AtomicU8`） | 热切 `Lock` / `Stage-Aware` 毫秒生效，无需重启正在运行的 Claude Code 终端 |
| 轨迹存储 (`TrajectoryStore`) | 本地 SQLite WAL 模式（`rusqlite` `bundled` 静态链接） | 敏感 Prompt 仅留存本机，不上传云端；流式结束后异步写入，零 TTFT 阻塞 |

## 2. 一键原子化 CLI 配置接管（CC Switch 模式）

`crates/gateway-core/src/cli_takeover.rs` 为三大主流终端编码工具提供无损接管与还原：

| 客户端 | 目标配置文件 | 接管写入字段 | 原子与回滚保障 |
|---|---|---|---|
| **Claude Code** | `~/.claude/settings.json` | `env.ANTHROPIC_BASE_URL` + `env.ANTHROPIC_AUTH_TOKEN` | 保留原有所有字段，先写 `.tmp.<pid>` 再 `fs::rename` 原子替换 |
| **Codex CLI** | `~/.codex/config.toml` | `openai_base_url = "http://127.0.0.1:8787/v1"` | 首次接管前自动复制 `.edge-gateway.bak` |
| **Gemini CLI** | `~/.gemini/settings.json` | `env.GEMINI_API_BASE_URL` | 点击 Restore 原子恢复备份并清理 `.bak` 文件 |

## 3. Append-Only 轨迹表结构与多模型 Fork & Diff

### 3.1 仅追加事件表（`trajectory_steps`）

每次请求（无论完整结束或被客户端 Ctrl+C 中断）均向 `trajectory_steps` 追加一条不可变记录，按 `workspace → session_id → turn_index → step_id` 串成时间线，并通过 `parent_step_id` 表达分叉分支：

* 关键指标列：`channel_id`、`requested_model`、`actual_model`、`route_mode`、`sticky_hit`、`ttft_ms`、`input_tokens`、`cache_read_tokens`、`output_tokens`、`status`（`completed` / `aborted_by_client` / `stream_error`）。
* 复现上下文列：`request_json`（完整原始请求快照）、`response_text`、`tool_calls_json`。

### 3.2 Fork & Diff 执行链路

当开发者在 Tauri 控制台（或调用 `POST /api/steps/:step_id/fork`）对第 `N` 步触发 **Fork → Target Model** 时：

1. 从 SQLite 读取原步骤 `original` 的 `request_json` 与协议类型；
2. 仅替换 `model` 为 `target_model`（强制 `RouteMode::Lock` 确保精确评测目标模型，不触发降级）；
3. 复用同一套 `GatewayEngine` 管道执行（完整享受四级熔断与跨协议无损翻译，例如原步骤是 Anthropic Claude，可直接 Fork 给 OpenAI `gpt-4o`）；
4. 将新结果以 `parent_step_id = Some(original.step_id)` 追加写入 SQLite，并返回 `ForkDiffResult`（包含原/新步骤文本、`ttft_delta_ms`、`output_tokens_delta`、`cache_hit_ratio_delta`）供前端并排渲染。

## 4. 阶段三演进预留

完成阶段二后，进入阶段三（云端化与微内核插件生态）只需两处增量扩展：
1. 在 `GatewayEngine` 请求前后插入洋葱模型 `Middleware` Trait 管道（脱敏护栏、高危工具策略拦截、MCP 聚合路由）；
2. 新增可选的云端同步器，仅将 `trajectory_steps` 中的脱敏元数据（剔除 `request_json` / `response_text`）异步上报至云端 Control Plane + ClickHouse，并接入 Redis Lua 双阶段原子记账。
