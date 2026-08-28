#!/bin/bash
# 被其它脚本 source，不要直接执行。
#
# 两件事不写死：
#   项目名  —— 从 Cargo.toml 的 [package] name 读，脚本丢进任何 Rust 项目都能用
#   编译档  —— target/release 和 target/debug 都认，因为你可能 cargo run
#              也可能 cargo run --release
#
# 覆盖方式：
#   APP=shell        ./sh/start.sh    一个项目有多个 bin 时指定跑哪个
#   PROFILE=debug    ./sh/start.sh    强制用 debug 版（默认挑较新的那个）

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

BIN_RELEASE="$PROJECT_ROOT/target/release/$APP"
BIN_DEBUG="$PROJECT_ROOT/target/debug/$APP"
LOG="$PROJECT_ROOT/$APP.log"

# 找进程时两个档都比对：不管当初是用哪个档起来的，stop 都该能停掉它
ALL_BINS=("$BIN_RELEASE" "$BIN_DEBUG")

case "${PROFILE:-}" in
    release) START_BINS=("$BIN_RELEASE") ;;
    debug)   START_BINS=("$BIN_DEBUG") ;;
    "")      START_BINS=("$BIN_RELEASE" "$BIN_DEBUG") ;;
    *)       echo "PROFILE 只能是 debug 或 release，当前是 '$PROFILE'" >&2; exit 1 ;;
esac

# 该启动哪个：存在的里面挑修改时间较新的 —— 你最近编的那个就是你想跑的
pick_bin() {
    local best="" b
    for b in "${START_BINS[@]}"; do
        [ -x "$b" ] || continue
        if [ -z "$best" ] || [ "$b" -nt "$best" ]; then
            best="$b"
        fi
    done
    [ -n "$best" ] && printf '%s\n' "$best"
    return 0
}

# 找出正在跑这个项目的所有 pid。
# 不用 pgrep：`pgrep -f` 会匹配到 tail/编辑器等命令行里含这个路径的进程，
# `pgrep -x` 又受 comm 15 字符截断的限制。直接比对 /proc/<pid>/exe 最准，
# 顺带也不怕 pid 被系统复用。重新编译过的进程 exe 会带 " (deleted)" 后缀。
#
# 注意 cargo run 会留下 cargo 和真正的二进制两个进程，这里只认后者；
# 杀掉子进程后 cargo 自己会退出。
running_pids() {
    local pid exe b found=""
    for pid in /proc/[0-9]*; do
        pid=${pid#/proc/}
        exe=$(readlink "/proc/$pid/exe" 2>/dev/null) || continue
        exe=${exe% (deleted)}
        for b in "${ALL_BINS[@]}"; do
            if [ "$exe" = "$b" ]; then
                printf '%s\n' "$pid"
                found=1
                break
            fi
        done
    done
    # /proc/<pid>/exe 读不到时（hidepid、非 root、容器里看不到别人的 /proc）
    # 退一步按二进制绝对路径匹配命令行。`tail -f xxx.log` 不含这个路径，不会误伤。
    if [ -z "$found" ]; then
        for b in "${ALL_BINS[@]}"; do
            pgrep -f -- "^$b( |$)" 2>/dev/null
        done
    fi
    return 0
}
