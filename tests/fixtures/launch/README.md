# 自动发现的固定测试项目

本目录是提交到仓库的测试输入，不需要安装依赖。

- `package.json`、`probe.cjs`：npm 的参数透传和工作目录探针。
- `pyproject.toml`、`probe.py`：标准 Python 脚本声明的调用、参数和工作目录探针。
- `go.mod`、`main.go`、`cmd/`：Go 根入口、命令入口、库包、嵌套 module 的发现边界。
- `app.jar`、`library.jar`：固定 ZIP 清单样本，分别有/无 `Main-Class`。只验证 JAR 入口发现，不含 Java class，不能用作真实 Java 启动验收。

运行普通测试使用 `cargo test --locked`。本机装有 Node/npm CLI 与 Python 时，另运行 `cargo test --locked auto_launch::tests::real_ -- --ignored`。Windows 还需将 `PANEL_TEST_POSIX_SHELL` 设置为已安装的 POSIX shell 路径，例如 Git 的 `sh.exe`；测试使用 npm CLI 和 POSIX 转义分支，不改变 panel 的生产命令。

`../launch-invalid/package.json` 是故意不合法的配置；`../launch-broken-venv/.venv/README.md` 保持一个缺少解释器的虚拟环境目录。这两处是错误分支的固定输入。

`../cargo-tooling/` 是带 Node 构建脚本的 Cargo 项目，用于验证没有 Node 启动脚本时保留 Cargo 的服务选择规则。
