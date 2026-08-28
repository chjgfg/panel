#!/bin/bash
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

BIN=$(pick_bin)
if [ -z "$BIN" ]; then
    echo "找不到可执行文件，这些位置都看过了："
    for b in "${START_BINS[@]}"; do echo "  $b"; done
    echo "先编译一次：cargo build --release   （或 cargo build 出 debug 版）"
    exit 1
fi

pids=$(running_pids)
if [ -n "$pids" ]; then
    echo "已经在运行，pid: $(echo "$pids" | tr '\n' ' ')"
    exit 0
fi

# 顺手告诉你跑的是哪个档，免得以为在跑 release 其实是 debug
case "$BIN" in
    *"/target/release/"*) prof=release ;;
    *) prof=debug ;;
esac
echo "启动 $APP（$prof）..."

# 用 >> 而不是 >：单个 > 每次启动都会把历史日志截断掉，出问题没法回溯
nohup "$BIN" >> "$LOG" 2>&1 &
child=$!

# 用 kill -0 直接问「这个子进程还活着吗」，不依赖 /proc 反查 ——
# 反查失败时会误报「启动失败」，而它其实起来了
sleep 0.3
if ! kill -0 "$child" 2>/dev/null; then
    echo "启动失败（进程立刻退出了），日志最后 20 行："
    tail -n 20 "$LOG" 2>/dev/null || echo "(没有日志)"
    exit 1
fi
echo "已启动，pid: $child"
echo "日志追加到 $LOG    查看: tail -f $LOG"
