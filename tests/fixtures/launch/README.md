# 自动发现的固定测试项目

本目录是提交到仓库的测试输入，不需要安装依赖。

- `package.json`、`probe.cjs`：npm 的参数透传和工作目录探针。
- `pyproject.toml`、`probe.py`：标准 Python 脚本声明的调用、参数和工作目录探针。
- `go.mod`、`main.go`、`cmd/`：Go 根入口、命令入口、库包、嵌套 module 的发现边界。
- `app.jar`、`library.jar`：固定 ZIP 清单样本，分别有/无 `Main-Class`。只验证 JAR 入口发现，不含 Java class，不能用作真实 Java 启动验收。

运行普通测试使用 `cargo test --locked`。本机装有 Node/npm CLI 与 Python 时，另运行 `cargo test --locked auto_launch::tests::real_ -- --ignored`。Windows 还需将 `PANEL_TEST_POSIX_SHELL` 设置为已安装的 POSIX shell 路径，例如 Git 的 `sh.exe`；测试使用 npm CLI 和 POSIX 转义分支，不改变 panel 的生产命令。

`../launch-invalid/package.json` 是故意不合法的配置；`../launch-broken-venv/.venv/README.md` 保持一个缺少解释器的虚拟环境目录。这两处是错误分支的固定输入。

`../cargo-tooling/` 是带 Node 构建脚本的 Cargo 项目，用于验证没有 Node 启动脚本时保留 Cargo 的服务选择规则。

新增固定输入索引：

- `../python-files/`：三个约定单文件入口；环境变量 `PANEL_EXPECT_ARGUMENT` 用于逐字符核对完整参数。
- `../java-maven/`：真实 Spring Boot Maven repackage 项目；`../java-gradle/`：真实 Kotlin DSL application 项目。二者共用相同的参数判据，各自保留完整可构建源码。
- `../java-ambiguous/build/libs/`：两个只有 Main-Class 清单的 ZIP，用于验证多产物拒绝分支；不作为实际 Java 运行证据。
- `../java-single/build/libs/app.jar`：复用上面的清单 ZIP，用于构建后 argv 顺序的对照测试，同样不含 Java class。
- `../node-service/`、`../go-probe/`：真实 Node/Go 前台进程探针。
- `../cargo-feature-probe/`：无门槛、单 feature、多 feature 的真实 Cargo bin；门槛缺失会编译失败。
- `../ui/panel.toml`：回环地址页面验收配置，从 panel 根目录运行。

Java 真实测试先执行 `cargo build --locked`，将 `PANEL_TEST_PANEL_BIN` 设置为同次构建的 panel 绝对路径，再运行 `cargo test --locked java_launch::tests::real_ -- --ignored --nocapture`。需要 JDK 与对应构建工具；只在样本中构建，不启动项目服务。Python/Go 的 `auto_launch::tests::real_` 测试同样按本机工具条件选择执行。

Linux systemd 实跑使用 `cargo test --locked lifecycle::tests::real_systemd -- --ignored --nocapture --test-threads=1`，需要 transient unit 权限和各语言工具链。测试只创建带 PID 和时间戳的 `panel-validation-*` unit，结束或断言失败时停止自身 unit；不接触线上项目 unit。
