# Rust MCGS 实现及上游来源

`go-core` 提供围棋规则、MCGS 搜索和数值处理。它不链接 C++ 搜索或推理库，也不需要 KataGo 源码即可编译。本 crate 包含依据 KataGo 源码直接移植的实现，使用 [MIT 许可证](LICENSE) 并保留上游归属；详见 [第三方声明](../../THIRD_PARTY_NOTICES.md)。

固定源码基准为 [KataGo `231e1c4b938f068628a5e3e59a3e842ad5fc92cd`](https://github.com/lightvector/KataGo/tree/231e1c4b938f068628a5e3e59a3e842ad5fc92cd)。版本化协议中的 `katago-eval-v1` 表示本项目约定的评估语义，不代表官方 KataGo 提供本服务的 gRPC 接口。

## 规则与局面身份

- 从空棋盘严格黑白交替；核心允许 2–19 路便于测试，服务接口限定 19 路。贴目为 -150 到 150 的整数或半整数。
- 规则对应 `Rules::parseRules("chinese")`：面积计分、simple ko、禁自杀、无税/按钮、friendlyPassOk、WHB_N；当前服务不支持让子，实际让子补偿为零。它不同于 Chinese-OGS 的 positional superko。
- situational 长循环第三次出现判无结果；pass 清理循环历史；连续两次 pass 和同色在相同局面重复 pass 可结束棋局。面积计算次序参考 `Board::calculateArea` 的 Benson pass-alive、pass-dead 及其余区域处理。
- 搜索内部依据上游 `shouldSuppressEndGameFromFriendlyPass`，允许普通第二次 friendly pass 的子节点继续评估，使用单独的图身份。第三次 pass、Spight 重复 pass、非法着法和无结果不会被放宽；实际棋局的终局行为保持不变。
- `BoardHistoryModes` 固定为 `(false, false)`，不采用 Worker 模型 metadata 的默认偏好。
- 图身份遵循 `GraphHash` 的 `repBound=11`：足够不可逆的落子重置为状态身份，其他落子和 pass 继续链接前驱身份。本实现使用 SHA-256，因此哈希字节与上游 Zobrist 不同。状态包含行棋方、simple ko、贴目、终局/连续 pass 以及 pass 能否结束阶段。
- 神经网络请求另外对完整可重放输入计算 SHA-256。共享节点使用首次有效评估的历史代表，不假定转置路径拥有逐字相同的 NN 特征。模型和输入配置必须在搜索上下文之间隔离。

## 搜索实现对应关系

`Search` 为单写者。节点评估前创建唯一 token，在途路径独立保存临时占用；等待共享节点不会额外增加 visits。失败释放占用，重试使用新身份；切根增加 generation，迟到、重复和已取消结果不能修改统计。

| 已实现内容 | 上游对应及语义 |
|---|---|
| 独立节点与边访问量 | `SearchChildPointer`、`NodeStats::childWeight` |
| 共享节点价值刷新 | `recomputeNodeStats`，幂等重算；刷新其他祖先价值时不复制 visits |
| 多价值量及权重 | win/loss、no-result、score mean / mean square、lead、utility / utility square、weight sum / squared sum；白方正视角 |
| 父边有效权重 | `child.weightSum * edgeVisits / child.visits`；平方权重按最终缩放系数平方调整 |
| 较差子节点降权 | `downweightBadChildrenAndNormalizeWeight`，Student-t(df=3) 的 2000 格插值 CDF；默认 exponent=0.5，降权后保持总权重 |
| PUCT 与 FPU | 父边有效权重、0.01 分子偏移、可选 log 系数、已访问策略质量 FPU 降幅和临时 virtual loss |
| 不确定性权重 | `computeWeightFromNNOutput`；默认关闭；旧模型通过能力标志回退单位权重，保留 -1 哨兵 |
| 分数效用 | `ScoreValue::expectedWhiteScoreValue`，相同积分及双线性插值；static factor=0.3、scale=2.0 |
| 转置追赶 | `maybeCatchUpEdgeVisits`，leak=0；父边可使用共享节点的既有访问量，无需新评估 |
| 路径内循环和终局 | 循环停止下探；真终局直接规则求值；整数终局目差的 mean-square 含 draw=0.5 对应的 0.25 格点项 |
| 候选与 PV | 按父边有效权重排序；PV 和 variation 只读取既有图 |

明确参考配置为：`useGraphSearch=true`、`graphSearchRepBound=11`、`graphSearchCatchUpLeakProb=0`；win/loss utility=1、no-result utility=0、draw=0.5；PUCT exploration=1、log=0、FPU reduction=0.2；value weight exponent=0.5；static score utility=0.3、dynamic score utility=0。root noise、noise pruning、subtree bias、eval cache、human-SL、mirror/passing hacks、LCB、root symmetry pruning 等可选功能未启用。此范围不等于完整复现 KataGo 默认生产配置。

本实现调整了执行流程：评估占用前移；异步等待期间尝试其他可执行边；结果到达后刷新受影响祖先；按节点数、内存预算、在途数和深度限制扩展。上游图身份本身使用有界历史启发式，本实现仍沿实际路径验证合法性与终局。分布式完成顺序可能改变搜索轨迹，不保证逐步等同上游多线程搜索。

## 资源与验证边界

图预算计入节点最大出边、逆向父引用、所有权、哈希表及在途历史等逻辑存储。`memory_bytes` 不等于进程 RSS；allocator、网络、临时遍历和数值缓存另有开销。预算不足时停止扩展；切根先取消任务，再回收不可达节点。

进程共享的数值缓存不包含棋局或模型状态：Student-t CDF 初始化一次，分数效用积分格点按需初始化。缓存不改变原公式的积分顺序、舍入、钳位或插值。普通测试保留缓存前公式作为独立对照，并检验边界、固定随机输入和并发访问。

```text
cargo test -p go-core --locked
cargo clippy -p go-core --all-targets --locked -- -D warnings
```

数值微基准默认 `ignored`，显式执行方式为：

```text
cargo test -p go-core --release --locked benchmark_numeric_tables -- --ignored --nocapture
```

公开包不包含 KataGo C++ oracle 驱动及对应外部对照测试。普通测试检查本项目的规则、搜索生命周期和数值一致性，不证明上游对照已经执行，也不证明棋力或 GPU 性能。

## 上游参考

- [GraphSearch 原理](https://github.com/lightvector/KataGo/blob/231e1c4b938f068628a5e3e59a3e842ad5fc92cd/docs/GraphSearch.md)
- [棋盘与面积计算](https://github.com/lightvector/KataGo/blob/231e1c4b938f068628a5e3e59a3e842ad5fc92cd/cpp/game/board.cpp)
- [局面历史与规则终局](https://github.com/lightvector/KataGo/blob/231e1c4b938f068628a5e3e59a3e842ad5fc92cd/cpp/game/boardhistory.cpp)
- [图身份](https://github.com/lightvector/KataGo/blob/231e1c4b938f068628a5e3e59a3e842ad5fc92cd/cpp/game/graphhash.cpp)
- [搜索流程](https://github.com/lightvector/KataGo/blob/231e1c4b938f068628a5e3e59a3e842ad5fc92cd/cpp/search/search.cpp)与[统计更新](https://github.com/lightvector/KataGo/blob/231e1c4b938f068628a5e3e59a3e842ad5fc92cd/cpp/search/searchupdatehelpers.cpp)
- [NN 输入及分数效用](https://github.com/lightvector/KataGo/blob/231e1c4b938f068628a5e3e59a3e842ad5fc92cd/cpp/neuralnet/nninputs.cpp)
