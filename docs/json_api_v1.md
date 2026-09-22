# 前端 JSON 协议 v1

状态：实现契约，2026-09-08。浏览器连接 `ws://127.0.0.1:8090/ws`（地址可配置）。

请求为 `{id:string,type:string,sessionId?:string,...字段}`；连接打开会话后可省略 sessionId。响应为 `{type:"response",id,ok:true,data}` 或 `{type:"response",id,ok:false,error:{code,message}}`。快照事件独立于响应，使用 `type:"snapshot"`。请求 ID 在同一连接内唯一；断线不自动重放修改请求。

| type | 字段 | 行为 |
|---|---|---|
| open | sessionId 可选 | 缺省创建会话，给定则恢复；失效返回 SESSION_NOT_FOUND；data 为完整快照 |
| snapshot | 无 | 获取完整快照 |
| new_game | komi 可选，rules 可选 | 清空棋局；省略配置时保留当前贴目与规则，新会话初始 7.5、中国规则 |
| configure | komi 可选，rules 可选 | 保留主线，以新配置重建兼容分析上下文 |
| configure_search | search:对象，generation 可选 | 原子更新本会话 PDA 与宽根参数；实际 NN PDA 改变时清图，仅宽根或等效参照方变化保留旧图；保留棋谱与分析开关 |
| play | color:1或2,index:0..360或null | 服务验证并落子；历史位置落子截断后续主线；null 为停一手 |
| undo | 无 | 撤销当前浏览位置的上一手并截断后续主线 |
| seek | position:非负整数 | 在已有主线中切换分析根，保留完整主线 |
| set_position | moves:[{color,index}],position 可选,komi/rules 可选 | 完整棋局原子校验与导入；position 默认主线末尾；失败不修改已有棋局 |
| analyze | enabled:bool,maxVisits/maxTimeMs 可选 | 启停持续分析；可指定本轮新增 visits 与时长预算 |
| genmove | color 可选,maxVisits/maxTimeMs 可选 | 分析后直接落子；异步响应为落子后的快照；默认新增256 visits、最长3000ms |
| variation | index:0..360或null,generation 可选 | 只读已有图 PV，返回 {available,moves,generation,version}；序列包含候选点 |
| cancel | requestId:string | 取消同一连接的挂起请求（如 genmove）；不关闭 WebSocket |

第一版只接受 boardSize=19、中国规则（`chinese`，显示中文由前端处理）。若 set_position 包含 boardSize，必须为19。moves 内 color 为 1 黑、2 白；顶端一行为 row=0，index=row*19+column。所有落子与棋局导入以服务确认结果为准。

完整快照字段：

```json
{
  "type":"snapshot","sessionId":"...","generation":1,"version":1,
  "boardSize":19,"board":[0],"moves":[],"position":0,"toPlay":1,
  "captures":{"black":0,"white":0},"settings":{"komi":7.5,"rules":"chinese","search":{"playoutDoublingAdvantage":0,"playoutDoublingAdvantagePla":"root","wideRootNoise":0}},
  "analysis":{
    "enabled":true,"status":"waiting_workers","reason":"No compatible inference workers",
    "root":null,"candidates":[],"visits":0,"nodesPerSecond":0,
    "graphNodes":1,"memoryBytes":0,"inFlight":0,
    "limits":{"maxMemoryBytes":34359738368,"maxNodes":100000000,"maxDepth":1000},
    "evaluationsCompleted":0,"transpositionHits":0,"catchUpVisits":0
  },
  "workers":[]
}
```

示例 board 省略为一个元素；实际始终为361个0/1/2。moves 为完整主线，position 为当前浏览到的手数，board/captures/toPlay 对应该前缀。名称与显示偏好由前端管理。

真实 `memoryBytes` 由图预算计费计算，示例零值仅为结构占位，不代表根节点无开销。快照另含 `terminal:null` 或规则终局对象（`kind:"score",white_minus_black` / `kind:"no_result"`）。`analysis.enabled` 表示用户分析意图；没有算力或暂时无订阅者时仍可为 true。节点数、NN 完成数和 visits 是不同指标，不能互换。

`analysis.limits` 返回本会话的内存、节点和深度上限。2026-09-22 起，容量停止的 `reason` 分别是 `configured search memory budget reached`、`configured search node budget reached`、`configured search depth budget reached`，`status` 仍为兼容值 `memory_limited`。在途任务清空后才永久停止；此时 `nodesPerSecond` 为 0，结果仍可查看。计费可能因下一项分配所需空间不足而在略低于上限时停止。

`GET /api/sessions` 返回 `{"sessions":[...]}`。每项包含 `sessionId/generation/position/enabled/status/reason/visits/nodesPerSecond/graphNodes/memoryBytes/inFlight/limits/reclamation`，不含棋盘与候选。读取最近一次已发布快照，不创建/订阅会话、不启动搜索、不延长保留期，也不扫描搜索图。可用 `Invoke-RestMethod http://127.0.0.1:8090/api/sessions | ConvertTo-Json -Depth 8` 诊断 GPU 空闲时 Server 是否仍派发任务。

换根时先取消旧 EvalToken 和虚拟占用，再在会话线程上完成可达标记、编号映射和父索引重建；已摘除图由独立线程分批释放。`memoryBytes` 包含活动树（含换根临时空间预留）、待回收节点与其容器容量、待完成请求。`graphNodes` 只统计活动节点，因此落子后节点数下降而内存计费暂未下降是正常的。队列最多包含两个运行中/待处理批次，并受当前会话剩余字节和节点预算约束。内存暂被回收占用时搜索等待后重试；不会永久锁定为 `memory_limited`。预算不足、少于1024个退休节点或回收线程启动失败时同步释放，故不承诺任意规模固定落子延迟。会话退出会排空并 join 回收线程。

`analysis.reclamation` 提供 `pending_nodes/pending_bytes/pending_batches/peak_pending_bytes`、累计 `reclaimed_nodes/completed_batches/synchronous_batches/drop_ms/backpressure_ms`。计费在整个批次释放后扣减，属于保守估算；后台可能在快照取样之间继续推进。`analysis.rootChange` 为最近一次核心换根的记录（尚未换根为 null），包含核心 `generation`、`old_nodes/retained_nodes`、`cancel_ms/mark_ms/compact_ms/relink_ms/index_ms/retire_ms/total_ms/workspace_bytes`，及从核心换根开始到首个新 NN 请求/有效完成的 `first_request_ms/first_completion_ms`（尚未发生为 null）。核心 generation 不等同于会话 generation。`total_ms` 不含网络与快照发布；客户端仍需测量命令往返和新代首帧。Server 的 `go_server::root_change` 日志另记停止任务、快照构造与确认耗时。

默认启用后台回收；`--synchronous-graph-reclamation` 可用于诊断对照，`/health.configuration.search.background_reclamation` 返回实际值。此开关只改变析构调度，换根剪枝算法相同。

`rootChange.first_analysis_ms` 是首个新有效结果应用后，Server 开始构造新分析快照的时间；尚未发生为 null。它不包含 WebSocket 发送和客户端绘制。普通回收每256个节点让出约1ms；队列/内存压力或退出时取消这段节奏延时、优先排空。`drop_ms` 是实际析构线程经过的墙钟时间，包含节奏延时，不能当成纯 CPU 耗时。

`analysis.status` 为 idle/analyzing/waiting_workers/memory_limited/finished/error。无 NN 或搜索结果时 root=null、candidates=[]，不填演示数据。root 为 `{winRateBlack,scoreLeadBlack}`；候选为 `{index,color,winRateBlack,scoreLeadBlack,visits,weight,prior,pv:[{color,index}]}`，未访问候选的 winRateBlack/scoreLeadBlack 为 null。weight 是父边有效权重；PV 含候选第一手，只有该边存在时可仅一手。胜率范围0..1，目差为黑方正值领先；无胜负在展示胜率中按中性0.5处理。传输的 scoreLeadBlack 表示搜索目差均值，名称保留前端消费接口。

generation 在切换根或分析任务代次变化时递增，version 单调递增。前端丢弃较旧 generation/version 的快照。快照默认约300ms合并发布；命令响应、停止和错误及时发送。短暂断线暂停无人订阅的搜索，服务保留会话供恢复；首版悬停不会新建推理任务。

兼容 Worker 未连接时，analyze 可进入 waiting_workers；genmove 无法取得有效评估时明确失败，不静默随机落子。错误码会区分 INVALID_REQUEST、ILLEGAL_MOVE、UNSUPPORTED、STALE_GENERATION、CANCELLED、NO_WORKERS、CAPACITY 和 SESSION_NOT_FOUND。

## 会话搜索参数（2026-09-22）

前端「AI 分析 → 搜索参数」或「偏好设置」发送：

```json
{"id":"tune-1","type":"configure_search","generation":1,"search":{"playoutDoublingAdvantage":1.0,"playoutDoublingAdvantagePla":"black","wideRootNoise":0.04}}
```

`search` 可仅包含需要修改的字段，其余保留。未知字段、null、字符串数值、非有限值、越界值和非法参照方均拒绝，失败不修改任何状态。`generation` 过期返回 `STALE_GENERATION`，重新打开设置后再修改。

| 字段 | 默认 / 范围 | 含义 |
|---|---|---|
| playoutDoublingAdvantage | 0 / −3～3 | 固定 PDA；正值假设参照方具有约 `2^PDA` 倍搜索量的棋力预期，负值相反；不改变实际预算 |
| playoutDoublingAdvantagePla | root / root、black、white | root 以当前搜索根的行棋方为准；black/white 固定参照颜色 |
| wideRootNoise | 0 / 0～5 | 根部探索强度；0 关闭，可从 0.04 开始。越大越偏向广泛探索 |

设置保存在 Server 会话中，刷新、断线恢复、落子、悔棋、导入和清枰均保留；新建另一会话恢复默认，Server 重启后不持久化。完整快照通过 `settings.search` 回显实际值。旧客户端可忽略新字段；新前端连接缺失该字段的旧 Server 时禁用应用按钮。

参数发生变化时增加会话 generation，取消在途任务和挂起的 genmove，保留棋盘和主线；恢复此前持续分析意图，并清除原本的分析预算。比较新旧参数相对固定黑方的有效 PDA，只有 NN 输入条件实际改变才清空旧 NN 结果、统计与候选。仅调整宽根，或从 root 切换为与当前根颜色相同的固定参照方，可保留搜索图。没有 Worker 时仍可配置并进入等待算力状态。完全相同参数属于 no-op。前端会清除旧参数下的本页胜率曲线。

PDA 的参照颜色在整次搜索中固定，传往 Worker 的数值相对每个叶子的 `next_player` 正负变换。root 模式在换根改变行棋方时也会丢弃旧树；固定黑／白方在上下文兼容时仍可复用子树。非零 PDA 纳入输入哈希，旧 token 不会因重建图而复用。

前端首次开启 PDA 默认固定打开设置时的当前颜色：黑走选 black、白走选 white，之后落子不会自动换方。已启用的显式 root 选项保持不变，界面注明“换方重算”。从 root 改为当前颜色的固定方不改变当下 NN 条件，新 Server 会保留当前树并允许后续落子复用。

`analysis.rootChange` 新增 `retained_visits`（换根完成时立即继承的根访问量，不随随后搜索累加）与 `reuse_reason`：`reused`、`pda_reference_changed`、`pda_changed`、`position_context_changed`、`position_not_searched`、`memory_budget`。前端显示继承量或实际重算原因；旧 Server 缺少字段时隐藏该提示。换根只保留该分支的子树，不能把旧根全部访问量视为新根访问量。

宽根搜索只影响当前根的选边：`prior^(1/(4*wideRootNoise+1))` 加上以 50% 概率出现的半正态探索奖励，位于 virtual loss 之后；不改 NN 输出、回传价值、展示先验或 PV 排名。根部使用标量随机评分，深层继续使用原 SIMD 路径。采用独立可复现随机流，不声称与 C++ 逐次采样轨迹或完整搜索等同。

参数语义参考 [KataGo Analysis Engine](https://github.com/lightvector/KataGo/blob/master/docs/Analysis_Engine.md) 与本地固定版本搜索实现；这里实现固定 PDA，不包含 GTP 的动态让子 PDA 自动调节。

## Worker 调度与观测（2026-09-17）

Server 的 CPU 搜索内核可用 `--search-simd auto|scalar|avx512` 选择，默认 `auto`。`/health.configuration.search.simd` 是请求值，`/health.configuration.searchSimdSelected` 是实际选择值。`auto` 在 CPU/OS 支持 AVX-512F 时使用8路 FP64候选评分，否则回退标量；显式选择不受支持的内核会在启动时失败。此开关不改变 Worker/GPU 后端、网络协议、规则或候选排序语义。

默认每会话搜索图预算为32 GiB（`max_memory_bytes=34359738368`），节点上限为1亿（`max_nodes=100000000`）；可通过 `--graph-memory-mib` 和 `--max-nodes` 覆盖，实际配置见 `/health.configuration.search`。两项上限独立生效，达到容量后仍报告 `memory_limited`。预算按需使用、按会话独立计费，并非整个进程的共享内存池或 RSS 硬上限。提高节点上限不会自动扩大内存预算；现有二进制可显式传入 `--max-nodes 100000000`，源码默认值需重新构建后生效。

`/health.configuration.workerScheduling` 返回实际启动配置。`/api/workers` 和快照中的 Worker 增加以下兼容字段；旧的 EWMA 字段仍保留。所有计数器以连接为边界，重连须按 `connectionId` 分段。

| 字段 | 含义 |
|---|---|
| capacity | Hello 声明的硬接纳容量，取消中的请求也占用 |
| targetInFlight | Server 为该 Worker 配置并裁剪到硬容量的目标窗口 |
| dispatchLimit | 当前派发上限；completion 启动探测期间最多4，获得4个成功样本后使用目标窗口 |
| scheduler | legacy 或 completion；默认 legacy，新模式须显式启用 |
| predictedCompletionMs | 按派发时在途数量分档的实测 RPC EWMA 估计；启动时为探测先验，不是保证 |
| suppliedThroughputRps | 最近至少1秒区间内的有效回包率；区间至少80%时间达到目标窗口的75%才报告，否则 null；包含缓存命中，不等于 NN rows/s |
| sameSessionDispatches / sharedSessionDispatches | 派发时该 Worker 没有其他会话在途 / 存在其他会话在途的累计次数；两者之和等于 assignedRequests |
| metrics | 消息字节与阶段分布，见下表 |

| metrics 字段 | 计量边界 |
|---|---|
| distributionsAgeMs | 本组分布开始读取至返回的年龄（毫秒）；棋局快照共享短期缓存，读取时满100ms便刷新，计算/锁等待可能使返回年龄超过100ms；管理接口每次重新采样 |
| requestBytesEnqueued | 成功进入 Server 发送通道的评估消息编码字节 |
| requestBytesStreamed | tonic 已从通道取走的评估消息编码字节；不保证已经上网 |
| resultBytesReceived / resultMessagesReceived | 当前连接收到的所有评估回包，含重复、退休和错误回包；旧连接忽略 |
| rpc | 身份匹配的唯一终态回包：成功入发送通道前到 Server 收到解码结果；包含退休和错误 |
| retiredRpc | 上述 rpc 中已取消、不再应用到搜索图的回包子集 |
| workerElapsed / workerQueue / context / evaluator | Worker 自报的阶段持续时间；可选阶段未执行/缺失时不补零 |
| sendQueue | Server 入队到 tonic 拉取响应流，不含 tonic 后续编码、HTTP/2 流控或网络 |
| actorWait | 有效任务结果进入 actor 邮箱到 actor 取出，不含更新搜索图的 CPU 时间；取消后尚在邮箱的旧代次结果也可能被取出 |

字节以当前 schema 的外层 Protobuf 编码长度加5字节 gRPC 消息头计算，不含控制消息、HTTP/2、TCP/IP、TLS、ACK、重传；接收侧不计未来 schema 的未知字段。没有启用消息压缩。

每个分布为固定上限2048条最近观测的精确 nearest-rank 分位数：`totalCount` 是连接累计样本数，`windowCount` 是当前窗口样本数，`meanMs/p50Ms/p95Ms/p99Ms/maxMs` 均属于该窗口。空分布为 null，真实零值保留。不能相减不同阶段的 p95，也不能把相邻窗口的分位数差当作阶段增量。没有逐请求同步日志。

棋局快照中的分布（含 `totalCount/windowCount`）允许上述短期延迟；消息/字节计数及 Worker 容量、在途、连接身份仍在每次读取时获取。`/api/workers` 每次返回新采样的分布。每个分布单独采样，整组指标不是同一瞬间的原子快照。分位数排序在调度池锁和样本记录锁之外执行，缓存以连接为边界，重连不复用旧连接的分布。

启动示例：

```text
go-server --worker-scheduler completion --worker-target-inflight 64 --worker-target worker-a=32 --worker-target worker-b=64
```

`--worker-target` 可重复指定不同 Worker。各目标必须为1..4096，实际裁剪到 Worker 硬容量。legacy 未设置目标时保持旧容量；completion 未设置目标时使用 min(64, capacity)。completion 的评分不除以 capacity，使用在途分档的 RPC 完成时间估计，并对等待派发的会话轮转；失活等待者20ms到期，取消立即移除。此版目标窗口是显式配置值，尚未自动扩缩窗、感知任务 deadline 或做跨会话 NN 去重。物理信用仍仅在终态回包或连接回收后释放。

协议 v1 心跳只有 NN rows/batches 总数，不能据此恢复逐批分布、单任务缓存命中或精确的取消尾部 NN rows。分阶段实验应在前后物理排空并取得新心跳后统计总量；这些缺失指标不能用估算值冒充。
