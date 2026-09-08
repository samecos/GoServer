# 前端 JSON 协议 v1

状态：实现契约，2026-09-08。浏览器连接 `ws://127.0.0.1:8090/ws`（地址可配置）。

请求为 `{id:string,type:string,sessionId?:string,...字段}`；连接打开会话后可省略 sessionId。响应为 `{type:"response",id,ok:true,data}` 或 `{type:"response",id,ok:false,error:{code,message}}`。快照事件独立于响应，使用 `type:"snapshot"`。请求 ID 在同一连接内唯一；断线不自动重放修改请求。

| type | 字段 | 行为 |
|---|---|---|
| open | sessionId 可选 | 缺省创建会话，给定则恢复；失效返回 SESSION_NOT_FOUND；data 为完整快照 |
| snapshot | 无 | 获取完整快照 |
| new_game | komi 可选，rules 可选 | 清空棋局；省略配置时保留当前贴目与规则，新会话初始 7.5、中国规则 |
| configure | komi 可选，rules 可选 | 保留主线，以新配置重建兼容分析上下文 |
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
  "captures":{"black":0,"white":0},"settings":{"komi":7.5,"rules":"chinese"},
  "analysis":{
    "enabled":true,"status":"waiting_workers","reason":"No compatible inference workers",
    "root":null,"candidates":[],"visits":0,"nodesPerSecond":0,
    "graphNodes":1,"memoryBytes":0,"inFlight":0,
    "evaluationsCompleted":0,"transpositionHits":0,"catchUpVisits":0
  },
  "workers":[]
}
```

示例 board 省略为一个元素；实际始终为361个0/1/2。moves 为完整主线，position 为当前浏览到的手数，board/captures/toPlay 对应该前缀。名称与显示偏好由前端管理。

真实 `memoryBytes` 由图预算计费计算，示例零值仅为结构占位，不代表根节点无开销。快照另含 `terminal:null` 或规则终局对象（`kind:"score",white_minus_black` / `kind:"no_result"`）。`analysis.enabled` 表示用户分析意图；没有算力或暂时无订阅者时仍可为 true。节点数、NN 完成数和 visits 是不同指标，不能互换。

`analysis.status` 为 idle/analyzing/waiting_workers/memory_limited/finished/error。无 NN 或搜索结果时 root=null、candidates=[]，不填演示数据。root 为 `{winRateBlack,scoreLeadBlack}`；候选为 `{index,color,winRateBlack,scoreLeadBlack,visits,weight,prior,pv:[{color,index}]}`，未访问候选的 winRateBlack/scoreLeadBlack 为 null。weight 是父边有效权重；PV 含候选第一手，只有该边存在时可仅一手。胜率范围0..1，目差为黑方正值领先；无胜负在展示胜率中按中性0.5处理。传输的 scoreLeadBlack 表示搜索目差均值，名称保留前端消费接口。

generation 在切换根或分析任务代次变化时递增，version 单调递增。前端丢弃较旧 generation/version 的快照。快照默认约300ms合并发布；命令响应、停止和错误及时发送。短暂断线暂停无人订阅的搜索，服务保留会话供恢复；首版悬停不会新建推理任务。

兼容 Worker 未连接时，analyze 可进入 waiting_workers；genmove 无法取得有效评估时明确失败，不静默随机落子。错误码会区分 INVALID_REQUEST、ILLEGAL_MOVE、UNSUPPORTED、STALE_GENERATION、CANCELLED、NO_WORKERS、CAPACITY 和 SESSION_NOT_FOUND。
