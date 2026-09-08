# 公开文档入口

从 [项目 README](../README.md) 开始构建和运行。公开版本提供 Rust 服务端与协议；真实分析需要另行提供兼容的外部评估 Worker。

| 文档 | 内容 |
|---|---|
| [JSON 协议 v1](json_api_v1.md) | WebSocket 请求、响应、快照、会话和错误行为 |
| [Worker 协议 v1](worker_protocol_v1.md) | gRPC 接入、握手、评估输入输出、取消和故障处理 |
| [Worker Protobuf](../proto/worker.proto) | 通信消息的唯一 schema |
| [核心实现及来源](../crates/go-core/REFERENCE.md) | 围棋规则、MCGS、数值行为和 KataGo 源码对应关系 |
| [开源发布说明](open_source_release.md) | 公开范围、检查与源码导出步骤 |
| [依赖许可证](dependency_licenses.md) | Cargo 依赖的版本和许可证元数据 |
| [Unlicense](../LICENSE) | 服务端、协议及自有文档和工具的许可证 |
| [go-core MIT 许可证](../crates/go-core/LICENSE) | 核心实现及所含 KataGo 移植部分的许可 |
| [第三方声明](../THIRD_PARTY_NOTICES.md) | 许可证范围和上游归属 |

公开包不包含 KataGo、Worker 实现、模型、前端、GPU 部署脚本或内部验收记录。默认测试中的前端集成检查和数值微基准为显式 `ignored`；构建成功与普通测试通过不代表外部推理或性能验收已经完成。
