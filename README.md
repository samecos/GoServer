# Go Server · 围棋分析服务端

Go Server 使用 Rust 实现围棋规则、Monte-Carlo Graph Search（MCGS）和会话管理，通过 WebSocket JSON 与 GTP 接入客户端，通过 gRPC 调度外部神经网络评估 Worker。搜索图由服务端维护，每个 Worker 只负责局面评估。

本开源版本包含三个 Rust crate、服务端测试和通信协议，**不包含 KataGo 源码、修改版 KataGo Worker、模型、GPU 配置、前端或本地部署数据**。服务端可以独立构建和启动；没有兼容 Worker 时，可以检查健康状态、管理会话和棋局，无法进行真实神经网络分析或生成着法。分析会进入 `waiting_workers`，无法取得有效评估的 `genmove` 会返回错误。

官方原版 `katago` 不直接提供本项目的自定义 gRPC 接口。使用分析功能需要自行实现或另行获得符合 [Worker 协议](docs/worker_protocol_v1.md) 的评估 Worker，仅安装官方 KataGo 或下载模型并不足够。

## 功能与范围

- 服务接口支持 19 路、中国规则、半目贴目、落子、悔棋、切换历史位置、持续分析和生成着法。
- WebSocket JSON 提供会话恢复、搜索快照、候选着法和只读 PV；GTP 可使用标准输入/输出或 TCP。
- MCGS 支持转置共享、异步在途任务、取消、租约、Worker 断线恢复和每会话图内存预算。
- 会话可配置固定 PDA（数值及当前行棋方／黑／白参照）和宽根搜索强度；参数由前端调整，通过 `configure_search` 原子生效并回显，默认均关闭。
- 中国规则对应固定 KataGo 基准的 `chinese` 预设：simple ko、面积计分和长循环无结果。日本规则、摆子、让子及复杂 SGF 导入不在当前服务范围内。

客户端可以根据 [JSON 协议](docs/json_api_v1.md) 自行实现。服务端只提供 API 和 WebSocket，不托管前端静态页面。

## 构建

建议使用当前 stable Rust 工具链，以及平台对应的链接器：Windows 使用 MSVC 工具链和 Visual Studio C++ Build Tools；Linux 使用 Rust 原生工具链和 C/C++ 构建工具。仅支持 Rust 2024 edition 不足以保证满足锁定依赖的版本要求，本项目尚未单独验证最低 Rust 版本。首次构建需要下载 Cargo 依赖。项目提交了 `Cargo.lock`，Protobuf 编译器由构建依赖提供，无需单独安装系统 `protoc`、KataGo、CUDA 或模型。

在仓库根目录执行：

```text
cargo build --release --locked
```

Windows PowerShell：

```powershell
.\target\release\go-server.exe --http 127.0.0.1:8090 --grpc 127.0.0.1:50051
```

Linux：

```bash
./target/release/go-server --http 127.0.0.1:8090 --grpc 127.0.0.1:50051
```

另开终端检查服务，Windows PowerShell 使用：

```powershell
Invoke-RestMethod http://127.0.0.1:8090/health
Invoke-RestMethod http://127.0.0.1:8090/api/workers
```

Linux 使用：

```bash
curl http://127.0.0.1:8090/health
curl http://127.0.0.1:8090/api/workers
```

客户端 WebSocket 地址为 `ws://127.0.0.1:8090/ws`。发送 `{"id":"1","type":"open"}` 可创建会话并获得初始快照；无需 Worker。

## 配置与 GTP

执行 `go-server --help` 查看全部参数；Windows 使用上述可执行文件的完整相对路径。

| 参数 | 程序默认值 | 作用 |
|---|---|---|
| `--http` | `127.0.0.1:8090` | HTTP 和 WebSocket 监听地址 |
| `--grpc` | `0.0.0.0:50051` | Worker 接入监听地址；上面的启动命令显式限定为本机 |
| `--model-sha256` | 未指定 | 固定外部 Worker 使用的模型文件 SHA-256 |
| `--graph-memory-mib` | `32768` | 每会话搜索图的逻辑内存预算（32 GiB），按需增长，非进程 RSS 限额 |
| `--max-nodes` | `100000000` | 每会话搜索图节点上限，与内存预算分别生效 |
| `--max-in-flight` | `128` | 每会话在途搜索路径上限 |
| `--max-sessions` | `4` | 会话数量上限 |
| `--session-retention-secs` | `120` | 无订阅者时的会话保留时间 |
| `--publish-ms` | `300` | 搜索快照发布间隔 |
| `--lease-ms` | `20000` | 评估任务租约 |
| `--gtp` | 关闭 | 启用标准输入/输出 GTP，日志写入 stderr |
| `--gtp-tcp` | 不监听 | 启用指定地址上的 TCP GTP |

32 GiB 是每个会话独立的搜索图预算，不会在启动时预分配，也不是整个 Server 的共享池或 RSS 硬上限；多个保留会话的预算可以累加。内存或节点上限达到后，搜索会保留结果并报告 `memory_limited`；默认节点上限为1亿。可以显式传入较小预算，例如 `--graph-memory-mib 512 --max-nodes 100000`。实际生效值见 `/health.configuration.search`，修改启动参数需要重启 Server。

2026-09-22 修复了“约 1.1M visits 后速度归零，但实际内存远未达到 32 GiB”的过度预算计费问题。升级 Server 后保留原 `--graph-memory-mib 32768` 即可，不需要新增开关。计费现在依据实际图数组容量并保留必要的初始化、换根及回收空间；前端显示计费预算和图节点数。只读 `GET /api/sessions` 可查看各会话的停止原因、visits、图节点、在途数和预算，不改变分析状态。详细字段见 [JSON 协议](docs/json_api_v1.md)。

未指定 `--model-sha256` 时，第一个通过握手校验的 Worker 固定该服务进程的模型哈希；Worker 离线不会清除它。更换模型需要重启服务，已有会话不会跨服务重启保存。

例如，在启动命令后添加 `--gtp`，即可在终端输入 `protocol_version`、`boardsize 19`、`play b D4`、`quit`。添加 `--gtp-tcp 127.0.0.1:8091` 可供 TCP GTP 客户端连接。支持的命令由 `list_commands` 和 `known_command` 查询；`genmove`、`kata-analyze`、`lz-analyze` 使用同一外部 Worker 池。没有有效搜索结果时不会随机落子或返回虚构评估。

当前接口使用明文连接，没有内置认证或 TLS。远程访问需由部署方提供可信网络、访问控制或 TLS 代理，并显式调整监听地址；`0.0.0.0` 是监听地址，不是客户端连接目标。

## 开发验证

```text
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
```

普通测试包含规则、搜索生命周期、网络和 GTP 检查；网络测试使用明确的合成评估夹具，不需要外部 Worker。依赖另一个前端仓库和 Node.js 的回归检查默认标记为 `ignored`，公开包不提供该前端。数值函数的 release 微基准也默认 `ignored`，可单独运行：

```text
cargo test -p go-core --release --locked benchmark_numeric_tables -- --ignored --nocapture
```

公开包不包含链接 KataGo C++ 的 oracle 驱动、构建工具或对应外部对照测试。普通测试通过不能说明这些外部验证已执行，也不能证明神经网络精度、棋力或多 GPU 加速效果。

## 源码导出

维护者从完整开发工作区生成公开源码快照时，使用 Python 3：

```text
python scripts/export_open_source.py --check
python scripts/export_open_source.py --output .build/open-source/go-server
```

导出工具按公开清单复制文件，不携带原仓库 Git 历史。发布应使用生成的目录，并审阅导出结果；不要直接公开包含本地部署资料的开发目录。公开文档入口见 [docs/README_PUBLIC.md](docs/README_PUBLIC.md)，发布步骤见 [开源发布说明](docs/open_source_release.md)。

## 许可证与来源

服务端、协议以及本项目自有文档和工具采用 [Unlicense](LICENSE)，允许复制、修改、商用和再分发，无需署名或公开修改源码。

`go-core` 包含依据 KataGo 源码直接移植的实现，保留 [MIT 许可证](crates/go-core/LICENSE) 及上游归属，分发这部分代码时须保留相应版权与许可声明。第三方 Cargo 依赖也遵循各自许可证；根许可证不改变第三方权利。

算法与规则基准为 KataGo 提交 `231e1c4b938f068628a5e3e59a3e842ad5fc92cd`。实现对应关系和差异见 [核心来源说明](crates/go-core/REFERENCE.md)，许可范围见 [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md)。
