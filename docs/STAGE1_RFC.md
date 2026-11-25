# 阶段一工程设计文档（Stage 1 RFC）：端云协同微内核 AI 网关核心内核

## 1. 架构目标与边界

阶段一聚焦**单机本地数据面内核（Local Edge Kernel）**的极致可靠性与低开销，目标是在 2–4 周内稳定跑通 Claude Code（Anthropic Messages）与 OpenAI 兼容客户端，并为阶段二的 **Tauri v2 系统托盘 + SQLite 轨迹回放**预留零改造成本的嵌入边界。

| 维度 | 阶段一设计决策 | 演进至阶段二（Tauri）的衔接方式 |
|---|---|---|
| Crate 划分 | `gateway-core`（纯库）+ `gateway-daemon`（Axum 可执行入口） | `src-tauri` 直接依赖 `gateway-core`，托管 `Arc<GatewayEngine>`，免去 IPC 序列化开销 |
| 协议适配 | Lossless Canonical IR 超集结构，拒绝 `N × N` 点对点翻译 | 新增 Gemini / Bedrock 协议只需实现 `ProtocolCodec` Trait |
| 流式转发 | 逐字节增量解析 SSE 帧（`SseFrameParser`），无全量缓冲 | 阶段二可在转发循环中异步旁路写入本地 SQLite WAL 轨迹表 |
| 止损机制 | `CancelPropagatingStream` 利用 Rust RAII `Drop` 触发 `CancellationToken` | 客户端 Ctrl+C 断开即刻关闭上游 TCP 连接，并用输出字节数兜底估算 Token |

## 2. Lossless Canonical IR 核心契约

为避免普通中转站常见的“丢缓存断点、丢思考签名、丢严格工具 Schema”问题，`CanonicalRequest` 将四类高阶语义作为一等公民显式建模：

| 字段 | 语义与保真机制 |
|---|---|
| `cache_control` | 在 `system`、消息 `ContentPart` 与 `StrictToolSchema` 上原样保留 `ephemeral` 断点及 `ttl` |
| `reasoning` | 统一承载 Anthropic `thinking.budget_tokens` 与 OpenAI `reasoning_effort` |
| `opaque_state` + `SignatureCache` | 捕获流式 `SignatureDelta` 并按 `prefix_key` 侧车缓存，防止多轮思考链断裂 |
| `StrictToolSchema` | 完整保留 `strict: true` 与完整 JSON Schema，OpenAI 流式编码强制注入 `stream_options: {"include_usage": true}` |
| `extra_fields` | 透传未识别的顶层扩展字段，防止上游新特性在网关层被静默裁剪 |

## 3. 前缀缓存亲和路由与 P2C 调度

多轮 Agent 编码任务中，传统轮询（Round-Robin）会在每轮切换上游渠道，直接击穿 KV Cache。本内核采用两级选路：

1. **前缀指纹生成**：优先读取 `X-Session-Id` / `metadata.user_id`；若缺省，则对 `system + tools + 前 N 轮消息`（默认 `N=4`）计算 SHA-256 前缀指纹 `pfx:<24-hex>`。后续追加新对话轮次时，指纹保持稳定不变。
2. **亲和绑定（Sticky Affinity）**：以 `actual_model::prefix_key` 查 `DashMap` 绑定表。只要绑定渠道健康且未过期（默认 1800s），连续多轮强制命中同一上游渠道。
3. **P2C 并发感知冷启动**：首次请求或原渠道熔断后，从健康候选池中随机抽取 2 个渠道，按 `score = (inflight + 1) × ewma_ttft_ms` 选择负载更低者并建立新亲和绑定。
4. **显式透明模式**：
   * `lock`（默认）：严格锁死请求模型，绝不静默降级。
   * `stage_aware`：仅在显式配置规则且请求携带 `X-Gateway-Task-Stage` 时按阶段映射模型。
   * 所有响应强制返回 `X-Gateway-Actual-Model`、`X-Gateway-Channel-Id`、`X-Gateway-Route-Mode`、`X-Gateway-Sticky-Hit`、`X-Gateway-Prefix-Key`。

## 4. 四级错误治理引擎（4-Tier Error Governance）

| 分级 | 触发条件 | 治理动作 | 亲和绑定处理 |
|---|---|---|---|
| **L1 瞬时抖动** | Soft 429（`overloaded` / `Retry-After <= 3`）、500/502/503/504/529 | 不踢出渠道，原地指数退避 1–3s（含抖动）重试最多 2 次 | 保持绑定，保护 KV Cache |
| **L2 速率限制** | 硬性 RPM/TPM 429 或 L1 重试耗尽 | 渠道进入短冷却（默认 60s 或遵循 `Retry-After`），无缝切至健康备用渠道 | 临时绕过冷却渠道 |
| **L3 配额/鉴权失效** | 401/403、`insufficient_quota`、`credit balance` | 渠道进入长隔离队列（默认 900s），触发告警并切换备用渠道 | 立即解绑（`unbind_affinity`） |
| **L4 语义/风控错误** | 400/422、`invalid_request_error`、`content_policy`、`schema` | 绝不重试其他渠道，规范化报错直接返回客户端，防止雪崩扣费 | 保持不变 |

## 5. 阶段一验收标准与测试覆盖

随仓库附带的 `tests/mock_upstream.py` 与 `tests/conformance_suite.py` 自动化验证四项核心指标：

1. **Prompt Caching 命中率**：10 轮连续 Agent 对话锁定同一渠道，第 2–10 轮上游 KV Cache 命中率达 100%。
2. **断流止损（Cancel Propagation）**：客户端在慢速 SSE 流中途强制断开 TCP，上游在 <1s 内观测到 `aborted_streams >= 1` 且 `completed_streams == 0`。
3. **四级熔断准确性**：L4 400 不触发跨渠道重试；L2 硬 429 自动切换备用渠道并返回 200。
4. **透明路由校验**：`lock` 模式拒绝降级；`stage_aware` 模式显式跨协议路由并如实写入响应 Header。
