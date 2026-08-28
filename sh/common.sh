#!/bin/bash
# 被其它脚本 source，不要直接执行。
# 这里不写死项目名：二进制名从 Cargo.toml 的 [package] name 读，
# 所以这套脚本丢进任何一个 Rust 项目的 sh/ 目录都能直接用。
# 一个项目有多个 bin 时用环境变量覆盖：APP=shell ./sh/start.sh

set -uo pipefail

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)
PROJECT_ROOT=$(cd "$SCRIPT_DIR/.." && pwd -P)
cd "$PROJECT_ROOT" || exit 1

if [ ! -f Cargo.toml ]; then
    echo "$PROJECT_ROOT 下没有 Cargo.toml，这套脚本要放在 Rust 项目的 sh/ 目录里" >&2
    exit 1
fi

# 只在 [package] 段里找 name，避免匹配到 [dependencies] 里的
APP="${APP:-$(sed -n '/^\[package\]/,/^\[[a-z]/{
    s/^[[:space:]]*name[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p
}' Cargo.toml | head -1)}"

if [ -z "$APP" ]; then
    echo "Cargo.toml 里读不到 [package] name，用 APP=名字 手动指定" >&2
    exit 1
fi

BIN="$PROJECT_ROOT/target/release/$APP"
LOG="$PROJECT_ROOT/$APP.log"

# 找出正在跑这个二进制的所有 pid。
# 不用 pgrep：`pgrep -f` 会匹配到 tail/编辑器等命令行里含这个路径的进程，
# `pgrep -x` 又受 comm 15 字符截断的限制。直接比对 /proc/<pid>/exe 最准，
# 顺带也不怕 pid 被系统复用。重新编译过的进程 exe 会带 " (deleted)" 后缀。
running_pids() {
    local pid exe found=""
    for pid in /proc/[0-9]*; do
        pid=${pid#/proc/}
        exe=$(readlink "/proc/$pid/exe" 2>/dev/null) || continue
        exe=${exe% (deleted)}
        if [ "$exe" = "$BIN" ]; then
            printf '%s\n' "$pid"
            found=1
        fi
    done
    # /proc/<pid>/exe 读不到的情况（内核开了 hidepid、非 root、或者跑在容器里
    # 看不到别人的 /proc）退一步用 pgrep 按完整路径匹配。
    # 匹配的是二进制绝对路径，`tail -f xxx.log` 之类不含这个路径，不会误伤。
    if [ -z "$found" ]; then
        pgrep -f -- "^$BIN( |$)" 2>/dev/null
    fi
    return 0
}
