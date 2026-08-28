#!/bin/bash
SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" &>/dev/null && pwd)
PROJECT_ROOT="$SCRIPT_DIR/.."
cd "$PROJECT_ROOT" || exit 1

PID_FILE="./app.pid"
echo "==== xau status ===="
if [ -f "$PID_FILE" ];then
    PID=$(cat "$PID_FILE")
    if ps -p "$PID" >/dev/null;then
        echo "✅ 正在运行 pid: $PID"
    else
        echo "⚠️ pid文件存在，但进程已死亡"
    fi
else
    PID=$(pgrep -f "target/release/xau")
    if [ -n "$PID" ];then
        echo "✅ 正在运行 pid: $PID (无pid文件)"
    else
        echo "❌ 未运行"
    fi
fi

echo ""
echo "---- last 15 log lines ----"
tail -n15 app.log
