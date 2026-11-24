# panel

Linux systemd 项目运行面板：启动、停止、重启、日志、资源采样和源码查看。

## 配置入口

- `panel.toml.example`：面板监听地址、登录密码、项目扫描目录等全局配置。
- `panel-run.toml.example`：项目启动入口示例，包含 Java、Node、Python、Rust、Go 的命令模板。
- `src/launch.rs`：项目配置解析、入口来源选择、运行时检查和 systemd-run 参数生成的实现及测试。
- `src/auto_launch.rs`：各语言的自动入口发现及测试。
- `src/lifecycle.rs`：停止证明、启动确认及独立 Linux systemd 探针。
- `tests/ui.test.cjs`：页面状态提示的行为验证，执行 `node --test tests/ui.test.cjs`。
- `tests/fixtures/launch/README.md`：固定测试输入的索引和本地运行方式。

每个被扫描的项目，可以在项目根目录放一个 `panel-run.toml`。从示例中仅保留这个项目需要的 `[[targets]]`，替换 `YOUR_*` 占位值后保存。刷新页面即可看到入口，无需重启 panel。

有该文件时只使用显式配置的入口；没有该文件时按下表自动发现。混合语言项目合并各类入口；未发现入口时仍可管理已有的同名 systemd 服务。

配置存在但读取、解析或校验失败，会在项目状态中显示具体错误，启动/重启接口也会返回错误，不会悄悄回退。显式配置或跨语言自动发现的入口优先展示，不会被同名手写服务遮住；纯 Cargo 项目保留原服务选择规则。

## 自动发现规则

| 项目 | 识别依据 | 生成的命令及边界 |
| --- | --- | --- |
| Rust | `Cargo.toml`、默认 bin 路径和显式 `[[bin]]` | `cargo run [--features 必要特性] --bin 名称`；只启用所选 bin 的 `required-features`，不默认开启全部 feature |
| Node | `package.json` 的 `scripts` | 列出 `start`、`dev`、`serve` 及 `start:*`、`dev:*`、`serve:*`；生成 `npm run 脚本 --` 或其他管理器的 `run 脚本`，不把 build/test 当作启动入口 |
| Python | `pyproject.toml` 的 `[project.scripts]`、`[tool.poetry.scripts]` | 通过 `python -u -c` 导入 `module:callable` 并调用；支持点分隔的 callable 和 Poetry console 表格声明，保留脚本名为 `sys.argv[0]` |
| Java | 项目根、`target/`、`build/libs/` 的直接子文件 `*.jar` | 只有主清单段声明 `Main-Class` 才生成 `java -jar 相对项目的路径`；支持清单续行，不把库 JAR 或命名段字段当作可执行入口 |
| Go | `go.mod`、根目录及 `cmd/` 下的 Go 源码 | 根 main 包生成 `go run .`；cmd main 包生成 `go run ./cmd/相对路径`；忽略测试文件、隐藏目录、vendor、注释和字符串里的伪入口，跳过嵌套 module |

Node 优先使用 `packageManager` 声明的 npm/pnpm/yarn/bun；否则依据 `package-lock.json`、`npm-shrinkwrap.json`、`pnpm-lock.yaml`、`yarn.lock`、`bun.lock`、`bun.lockb` 选择。未声明且多种管理器锁文件共存时报错；都没有时使用 npm。声明的版本用于识别管理器，不负责安装或切换版本。自定义脚本名不在上述系列内时，可用显式配置。

Python 优先使用项目 `.venv` 的解释器；环境存在却缺解释器时直接报错。没有 `.venv` 时在 Linux 使用 `python3`，Windows 本地测试使用 `python`。包和依赖须已安装到所选环境，源码采用 src 布局时同样如此。脚本引用按 `module:callable` 处理；Poetry file 类型、extras、GUI 脚本和没有声明的 `main.py` 不自动猜测，可通过显式配置启动。

Java 需要先完成 Maven/Gradle 等打包；仅有 `pom.xml` 或 Gradle 构建文件时不假定项目能运行。JAR 清单损坏或无法读取时明确报错，清单读取限制为 64 KiB。Go 使用静态源码发现候选，不在发现时执行工具链；`go.mod` 检查唯一 module 声明，完整语法、构建标签、平台文件选择和依赖有效性由启动时的 Go 工具链判断。cmd 目录扫描最多 16 层，不沿其子目录的符号链接递归。

自动入口名带语言前缀，如 `node-start`、`python-worker`、`go-main`、`java-app.jar`。冒号、斜杠、下划线和其他特殊字符按 UTF-8 字节转为 `_hh`，因此 `dev:api` 对应 `node-dev_3aapi`，`cmd/api` 对应 `go-cmd_2fapi`；原始脚本名及路径传给工具时保持原样。编码后超过 100 字节或与 Cargo bin 重名时报错，可用显式配置指定短名称。

发现过程只读文件，不安装依赖、不构建、不启动任何候选。没有可确定的入口时不会自动执行项目命令；显式配置可以覆盖所有上述规则。

## 每个启动入口的字段

| 字段 | 类型 | 规则 |
| --- | --- | --- |
| `name` | 字符串 | 项目内唯一，仅 ASCII 字母、数字、点、下划线、连字符；不能以点或连字符开头 |
| `program` | 字符串 | 只写可执行程序；支持绝对路径、相对工作目录的路径、PATH 中的程序名 |
| `args` | 字符串数组 | 默认空；每个元素都是一个独立参数，空字符串也保留 |
| `working_dir` | 字符串 | 默认 `.`；相对于项目根目录，或填绝对目录 |
| `env` | 字符串键值表 | 默认空；环境变量名使用字母、数字、下划线，首字符不能为数字 |

可配置多个入口，每个入口使用独立的 `panel-项目名-入口名.service`，分别启停、查看日志和资源，重启沿用该入口上次填写的参数。程序必须在前台运行，不要自行后台化。

配置里的 `args` 与面板参数框分别处理：前者是参数数组；后者仍整串原样追加为一个参数，空白输入不追加。引号、空格、换行不会按 shell 语法拆分。多个常规选项应放进配置的 `args` 数组。

显式命令不会自动插入 `--`：npm 或 Cargo 等工具需要分隔符时，在 `args` 数组末尾写 `--`。Rust 自动发现模式继续自动把程序参数放在 Cargo 的 `--` 后；Node 自动发现只为 npm 添加分隔符，pnpm/yarn/bun 直接转发脚本名后的参数。Java、Python、Go 将完整字符串放在入口参数后。显式命令里的 Cargo feature 由 `args` 明确指定；自动发现模式读取 `required-features`。

panel 到运行工具的参数始终以 argv 传递，不经过 shell。Node 包管理器执行脚本时会使用自己的 shell；本地实测 Windows npm 默认 `cmd.exe` 会截断含换行的参数，POSIX shell 分支完整保留。项目自定义的 script-shell 或脚本内部再拆分参数，仍由该脚本负责。

工作目录在启动前检查。相对 `program` 路径从该工作目录解析；裸程序名从入口的 `env.PATH` 查找，未指定时使用面板进程的 PATH。未显式设置 PATH 时，会把解析出的程序所在目录补到启动 PATH 前面，便于程序寻找同目录工具。Node 版本管理器、自定义 JDK 和虚拟环境建议填写实际程序路径，或明确配置 PATH。Java 的 JVM 选项可通过 `JAVA_TOOL_OPTIONS` 传递；Python 可使用 `-u` 或 `PYTHONUNBUFFERED` 及时输出日志。

panel 自己启动不再要求安装 Cargo。只在启动 Rust 自动发现的入口时检查 Cargo，其余入口检查各自的可执行程序。构建和依赖安装由项目自行完成；需要复合流程时，可以把真实的前台脚本作为入口，并明确配置解释器及脚本参数。

整批入口在停止旧实例前检查配置、入口名称、工作目录和可执行程序。检查失败保留旧实例。配置损坏、删除或入口移除时，仍根据上次启动记录查询旧 unit；仍被 systemd 加载的旧实例会保留在列表中，可停止和查看日志，启动/重启按钮禁用。历史入口在 unit 已消失后不再占页面行，也不会自动重新启动；未曾由面板记录的实例不能凭损坏配置推断。面板外进程的现有识别仍限于 Rust 的 target 目录，多语言解释器进程不会被猜测归属。

## 验证与边界

项目操作与单入口操作共用项目异步锁，同项目按顺序执行，不同项目可并行。停止命令成功后仍复查 PID 和状态；停止失败或无法确认停止时不创建新实例。仅有效的 `LoadState=not-found` 可以视为已不存在，错误文本不能充当证明。启动后只有确认 active 才返回 HTTP 204；状态查询失败或持续启动中返回 HTTP 202，页面显示黄色“状态未确认”；确定失败返回 HTTP 500。未知和过渡状态单独显示，并保留停止入口。

在项目目录执行 `cargo test --locked`、`cargo check --locked` 和 `cargo build --locked`。

测试覆盖入口来源选择、配置错误、五类命令、参数边界、环境变量、工作目录、运行时缺失、Cargo feature、旧实例管理、配置往返序列化及自动发现。真实 Node/Python 探针作为可选测试，运行方式见测试输入索引。本地子进程验证不经过 systemd；Linux systemd 的实际创建、cgroup 管理和日志读取需要在服务器另外验收。

验收账本（2026-11-25，Windows）：

- 显式配置阶段（提交 `2e3cc0d`）：65 passed，0 failed；包含配置示例往返和真实本地子进程的环境、工作目录、参数检查。
- 本轮 `cargo check --locked`、`cargo build --locked`：通过。测试编译曾出现增量缓存目录访问拒绝警告，但测试退出码为 0；编译检查仅在当前命令中设置 `CARGO_INCREMENTAL=0`，没有修改系统配置。
- 显式配置阶段 `node --check static/app.js`：通过；实际 `syncBinRows` 函数的三个启停状态组合通过，字段缺省兼容旧响应，日志入口保留。本轮未修改前端或请求参数协议。
- `cargo clippy --locked --all-targets`：退出码 0，保留四处原有警告；加 `-D warnings` 的严格检查未通过。对应语句在改动前版本中也存在，未扩大范围修改。
- 显式配置阶段对照：移除参数追加和 systemd 环境注入后，两个相关测试失败；恢复后全量通过。原状态测试的共享临时文件名冲突已隔离。
- 自动发现阶段 `cargo test --locked -- --include-ignored`：83 passed，0 failed，0 ignored；Windows 真实 npm 探针使用 `PANEL_TEST_POSIX_SHELL` 指定本机 Git 的 sh，包含真实 Python 和 npm 子进程、混合项目、包管理器、Python 引用、JAR 清单、Go 伪入口及错误分支验证。普通测试默认跳过两个需外部运行时的探针。
- 本轮对照：移除名称转义的防碰撞规则及 Go main 包双重约束，两个测试失败；恢复后全量通过。JAR 相对扫描路径的回归测试在修正前失败、修正后通过；JAR 参数现在相对于项目工作目录解析。
- 修订：显式配置阶段只验证五类命令模板；本轮增加真实 Python 调用和 npm/POSIX shell 透传验证。Windows npm/cmd.exe 的换行截断已实测确认，未降低断言要求。Java/Go 实际运行和服务器 Linux systemd 尚未验证。
- 启停可靠性阶段：`cargo test --locked -- --include-ignored` 为 95 passed，0 failed；`cargo check --locked` 通过；`node --test tests/ui.test.cjs` 为 2 passed。覆盖停止失败阻断、停止后复查、未知状态、结果聚合、HTTP 状态以及两个真实 handler 等待同一项目锁。
- 可靠性对照：分别移除停止阻断、将未确认返回改为成功、移除 handler 锁后，三个相关测试均失败；源码按字节恢复。Linux 实际启停尚待本轮后续验收。

移除本项目的显式启动配置会恢复自动发现；配置变更不会自动停止已运行实例。回退本轮自动发现功能可恢复提交 `2e3cc0d` 后重新编译部署，该版本仍支持显式多语言配置和 Rust feature 修复。
