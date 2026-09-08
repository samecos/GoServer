# 外部评估 Worker 协议 v1

本项目定义 `goeval.v1.WorkerService/Connect` gRPC 双向流，唯一 schema 为 [worker.proto](../proto/worker.proto)。Worker 主动连接服务端，由服务端通过同一条流下发评估任务。公开包包含协议和 Rust 调度器，不包含 Worker 实现。官方原版 KataGo 不直接提供此接口，必须自行实现或另行获得兼容 Worker。

版本常量见 [go-protocol](../crates/go-protocol/src/lib.rs)，实际调度及校验见 [worker.rs](../crates/go-server/src/worker.rs)。`katago-eval-v1` 的规则和数值基准为 KataGo 提交 `231e1c4b938f068628a5e3e59a3e842ad5fc92cd`，详细来源见 [核心说明](../crates/go-core/REFERENCE.md)。Worker 必须提供真实的兼容评估结果，仅填入相同的协议字符串并不能保证兼容。

## 连接与握手

Worker 在连接后的 5 秒内发送第一条 `WorkerMessage`，其 payload 必须为 `hello`。服务返回 `Welcome`，包含 `protocol_version=1` 和本次 `connection_id`；之后 Worker 接收 `evaluate`、`cancel` 或 `drain`，发送 `result` 和 `heartbeat`。所有消息共用该流，任务允许乱序完成。

当前服务端在握手时强制校验：

| 字段 | 要求 |
|---|---|
| `protocol_version` | `1` |
| `input_profile` | `katago-eval-v1` |
| `supports_friendly_pass_search` | `true` |
| `worker_id` | 非空，UTF-8 字节长度不超过 128；同时运行的 Worker 应使用不同 ID |
| `instance_id` | 非空，标识 Worker 实例 |
| `model_sha256` | 64 个十六进制字符，与该服务进程固定的模型哈希一致，比较时忽略大小写 |
| `max_in_flight` | `1..4096`，可接纳的并发评估请求上限 |
| `max_board_size` | 至少 `19` |

`--model-sha256` 未指定时，第一个通过校验的 Worker 固定服务模型；此后断线不会清除模型身份。相同 `worker_id` 的新连接会替换旧连接并回收其任务，因此不要让不同 Worker 共用 ID。处于故障冷却期的 Worker 暂时不能重连。

`engine_commit`、`backend_info`、`model_version`、`supports_ownership` 和两个 history mode 默认值在 schema 中用于能力或来源描述，**当前注册逻辑不会据此拒绝连接**。部署方须确保来源与评估语义兼容。服务会校验每次成功结果的 `has_shortterm_error` 与连接声明 `supports_shortterm_error` 一致。`backend_info` 用于诊断，不用于选择 GPU 后端。

当前传输使用明文 gRPC，没有内置认证或 TLS。Worker 只需能连接到服务端，无需在 Worker 主机开放业务监听端口。跨主机部署需自行设置可信网络及访问控制。

## 评估请求

任务由 `(session_id, generation, task_id)` 关联；`input_hash` 是服务端输入身份，Worker 必须原样回显，不能替换成自己的 NN 缓存键。请求包含 `model_sha256`、相对租约 `lease_ms`、完整棋局和语义参数。

当前服务发送的棋局为 19 路 `chinese` 规则、空盘黑先，无初始摆子；贴目为 -150 到 150 的整数或半整数。`moves` 携带完整严格交替历史，`next_player` 必须与重放一致。颜色为 `BLACK=1`、`WHITE=2`，落点为左上原点的 `y*19+x`，pass 为 `-1`。Worker 应重放并验证上下文，不能静默纠正错色、非法着法或历史。

当前服务明确发送以下评估参数：

| 参数 | 值 |
|---|---|
| `symmetry` | `0` |
| `policy_temperature` / `policy_optimism` | `1` / `0` |
| `draw_equivalent_wins_for_white` | `0.5` |
| `playout_doubling_advantage` | `0` |
| `max_history` | `1000` |
| `include_ownership` / `skip_cache` | `false` / `false` |
| `always_compute_pass_alive` / `exclude_territory_adjacent_to_atari` | `false` / `false` |
| `conservative_pass` / `enable_passing_hacks` / `avoid_mytdagger_hack` | 均为 `false` |

`allow_terminal_search_history` 和 `force_non_terminal` 由搜索路径决定。前者仅允许重放跨过先前的普通第二次 friendly pass 终局；后者仅允许当前最终局面在同一上游谓词下继续搜索评估。它们不允许终局后任意继续、错色、非法着法、第三次 pass、Spight 重复 pass 或循环无结果。实际棋局的两次 pass 终局不因此改变。

这些是影响输入和输出含义的语义参数。GPU 后端、设备、批次与精度由 Worker 部署方负责；`max_in_flight` 是应用并发容量，不等于 GPU batch。

## 结果与数值约定

`EvalResult` 必须回显任务身份、输入 hash 和模型 hash。成功时 `error_code` 为空且提供 `output`；失败时提供明确的 `error_code` 和 `error_message`。身份不匹配会使服务移除连接；旧连接、重复或已经失效的结果不能更新搜索图。

`NNOutput` 是后处理后的评估结果：

- `policy` 为 362 个值，前 361 个按原棋盘方向逐行排列，最后为 pass；非法点可保留负值。服务不再次 softmax。
- 胜、负、无结果概率及所有 score/value 字段均为白方视角。三种结果概率之和应约为 1；分数必须有限，均值平方与二阶矩须一致。
- `has_shortterm_error` 必须与握手能力声明一致。旧模型缺少误差输出时用 `false` 表明，不把缺失值伪造为零不确定性。
- 当前请求不要求 ownership，可返回空数组；若提供则应为 361 个白方正视角、范围约 `[-1, 1]` 的值。

形状、数值范围和二阶矩的具体边界由 [Evaluation::validate](../crates/go-core/src/value.rs) 定义。服务端展示时会转换为 JSON 或 GTP 所需视角；Worker 不应提前转换成黑方或当前执子方视角。

## 取消、租约与故障

Worker 应限制队列和活动请求，及时处理 `Cancel`；取消不要求中止已运行的 GPU kernel，但应丢弃该评估输出并返回 `CANCELLED`。收到 `Drain` 时应停止接单并结束旧流。服务在取消后保留该请求占用的物理容量，直到结果/取消回执到达或连接被移除，避免超额提交。

服务使用自身计时器处理租约。取消后仍未结束的任务超过一个租约宽限期（至少 1 秒），会导致连接被移除；持续心跳不能无限保留卡住的已取消任务。断流后旧任务不能自动重放到新流。

| 错误码 | 服务处理 |
|---|---|
| `INVALID_CONTEXT` | 确定的输入错误，停止当前分析并报告，不无限重试 |
| `EVALUATOR_ERROR`、`INVALID_OUTPUT`、`WORKER_INTERNAL_ERROR` | Worker 故障，可重新调度；连续故障累计 |
| `EVALUATION_ERROR` | 兼容旧版本的未分类 Worker 故障 |
| `CANCELLED`、`LEASE_EXPIRED`、`CAPACITY_EXCEEDED`、`DRAINING` | 单独处理，不计入连续 evaluator 故障计数 |

其他非空错误码也作为可重试 Worker 故障计数。连续三次此类故障会断开该 Worker 并冷却 10 秒；成功评估清除连续故障计数。其他可用 Worker 继续工作。

Worker 应定期发送 `WorkerHeartbeat`；服务在超过 15 秒没有收到该 Worker 消息时移除连接。心跳报告活动请求、累计完成/失败和真实 NN rows/batches，不能用搜索 visits 代替 NN 推理计数。

`elapsed_us` 从 Worker 接纳任务开始计至 protobuf 输出或错误构造完成，包含队列、上下文和评估处理，不含出站队列及网络；它不是 RPC RTT。可选 `queue_us`、`context_us`、`evaluator_us` 分别记录排队、重放/准备和 evaluator 调用墙钟时间；缺失表示未提供或阶段未执行，零表示真实测得不足 1 微秒。evaluator 时间可能包含缓存、内部排队与后处理，不能称为 GPU kernel 时间。
