# 许可范围与第三方来源

根目录 [LICENSE](LICENSE) 使用 [Unlicense](https://unlicense.org/)，适用于本项目自有的 `go-server`、`go-protocol`、`proto/worker.proto`、公开文档和导出工具。允许商用、修改、闭源、出售和再分发，不要求署名或公开修改后的源码。软件按原样提供。

## go-core 与 KataGo

`crates/go-core/` 保留原有 **MIT** 许可，完整条款见 [crates/go-core/LICENSE](crates/go-core/LICENSE)。该许可同样允许商用、修改和闭源，但分发代码或其重要部分时需要保留版权及许可声明。根目录 Unlicense 不替代这项许可。

核心实现参照 [KataGo](https://github.com/lightvector/KataGo) 提交 `231e1c4b938f068628a5e3e59a3e842ad5fc92cd`。数值积分、插值及搜索降权实现与上游有明确对应关系，因此保留来源与上游 MIT 声明；不能仅因改用 Rust 就将这些内容整体视为不受第三方条款约束的代码。对应关系见 [核心参考说明](crates/go-core/REFERENCE.md)。

上游版权声明为：

> Copyright 2025 David J Wu ("lightvector") and/or other authors of the content in this repository.
> (See 'CONTRIBUTORS' file for a list of authors as well as other indirect contributors).

此处 `CONTRIBUTORS` 指 [KataGo 在对应提交中的贡献者名单](https://github.com/lightvector/KataGo/blob/231e1c4b938f068628a5e3e59a3e842ad5fc92cd/CONTRIBUTORS)。原始许可见 [KataGo LICENSE](https://github.com/lightvector/KataGo/blob/231e1c4b938f068628a5e3e59a3e842ad5fc92cd/LICENSE)。

公开源码包不含 KataGo 源码树、自定义 `nnworker` 实现、Worker 构建部署文件、模型或引擎/oracle 二进制；保留 Rust 服务端调用外部 Worker 所需的通信契约。排除引擎发行文件不会取消现有 Rust 核心的来源与许可说明。

## Cargo 依赖

依赖版本由 `Cargo.lock` 固定，完整元数据清单见 [依赖许可清单](docs/dependency_licenses.md)。这些第三方依赖使用各自的许可，根目录 Unlicense 不覆盖它们。源码包未内嵌 crates.io 依赖源码。

`protoc-bin-vendored` 在构建期间提供 Protobuf 编译器；独立构建服务端无需本地 KataGo 或 CUDA。若另行发布编译后二进制，需要按实际目标平台、功能和链接的依赖附带相应许可及声明，不能仅附带根目录 LICENSE。
