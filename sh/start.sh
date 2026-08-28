#!/bin/bash
# 脚本所在目录，向上回到项目根目录
SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" &>/dev/null && pwd)
PROJECT_ROOT="$SCRIPT_DIR/.."
cd "$PROJECT_ROOT" || exit 1

BIN="./target/release/xau"
LOG="./app.log"

if [ ! -f "$BIN" ];then
    echo "二进制不存在，请先执行 cargo build --release"
    exit 1
fi

PID=$(pgrep -f "target/release/xau")
if [ -n "$PID" ];then
    echo "程序已经在运行, pid=$PID"
    exit 0
fi

echo "启动 xau strategy ..."
nohup "$BIN" > "$LOG" 2>&1 &
echo $! > app.pid
echo "已启动，pid=$(cat app.pid)，日志: $LOG"
echo "查看日志: tail -f $LOG"
