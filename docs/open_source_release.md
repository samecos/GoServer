# 公开源码范围与导出

本发行面向 Rust 围棋服务端。精确的公开文件集合由根目录 `.open-source-files.txt` 指定；新增文件需要经过内容检查后显式加入。`.gitignore` 额外隔离本地引擎、模型、机器配置和构建产物。

## 包含与排除

包含三个 Rust crate 的服务端源码、普通测试、Worker Protobuf、公开 API/接入说明、许可声明、依赖元数据清单、CI 配置与导出工具。

排除 `KataGo/`、`worker/`、`models/`、`reports/`、本机运行环境、GPU 调优与部署脚本、私人棋谱、历史验收记录、日志、二进制、C++ oracle 及其专用测试。Rust 服务端的 Worker 接入代码和 `katago-eval-v1` 语义契约仍然保留。

`go-core` 中的上游参考及移植实现保留 MIT 许可；其他自有公开代码采用 Unlicense。具体范围见 [第三方说明](../THIRD_PARTY_NOTICES.md)。本发行没有宣称彻底移除了所有与 KataGo 有来源关系的 Rust 实现。

## 本地生成

要求 Python 3.11 或更高版本，仅使用标准库。在项目根目录执行：

```sh
python scripts/export_open_source.py --check
python -m unittest discover -s scripts -p test_export_open_source.py
python scripts/export_open_source.py --output .build/open-source/go-server
```

输出目录必须不存在；再次生成时选择新的目录名。工具只复制精确清单中的文件，并生成 `SHA256SUMS`。源文件、现有输出和本地引擎文件不会被删除；工具不初始化仓库、不提交、不推送。常见凭据扫描仅是一项辅助检查，不能证明不存在所有形式的敏感信息。

## 独立验证

在导出的目录中执行：

```sh
cargo build --workspace --locked
cargo test --workspace --locked
python scripts/export_open_source.py --check
```

普通构建和测试无需 KataGo、模型或 GPU。显式忽略的外部前端集成测试、数值微基准，以及未发行的上游 oracle 均不计为通过。测试中的模拟 Worker 只验证服务协议及调度行为，不代表真实神经网络推理、棋力或双 GPU 性能验收。

按 [README](../README.md) 启动服务，检查 `/health`；未连接兼容 Worker 时应能管理棋局并报告等待状态，不能生成真实分析。公开包不提供推理 Worker；上游原版 KataGo 不能直接代替此协议的 Worker。

CI 配置对 Windows 与 Linux 执行清单校验、导出工具测试以及 Rust 构建和普通测试。首次推送后的远端运行结果应以 CI 页面为准；提供工作流文件不等于远端 CI 已执行。

2026-09-08 已在不含引擎目录的独立导出副本上完成以下本地验证：

| 环境 | 构建与普通 Rust 测试 | 导出工具测试 |
| --- | --- | --- |
| Windows，Rust 1.97.0 | `cargo build/test --workspace --locked` 通过；57 通过、2 ignored | 18 通过，4 项符号链接测试因权限跳过；实际 junction 检查通过 |
| Ubuntu 24.04 WSL，Rust 1.97.1 | 同上；57 通过、2 ignored | 21 通过，1 项 Windows 专用 junction 测试跳过；实际符号链接检查通过 |

两边均额外验证了零 Worker 启动后的 `/health`、`/api/workers`、GTP 协议版本、合法落子和正常退出。构建产物放在导出目录之外，不随源码包发行。本次验证使用 dev/test 配置，没有执行真实推理、release 性能测试或远端 CI。

## 依赖更新与首次发布

更新 `Cargo.lock` 后，用 `cargo metadata --locked --format-version 1` 重新生成或核对 [依赖许可清单](dependency_licenses.md)。二进制发行应另外收集实际随附依赖的许可文本。

首次发布以独立导出目录作为仓库来源，先核对 `.open-source-files.txt`、`SHA256SUMS` 和许可范围，再连接目标远端。导出不携带本地 KataGo 仓库及其 Git 历史。
