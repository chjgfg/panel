#!/bin/bash
SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" &>/dev/null && pwd)
PROJECT_ROOT="$SCRIPT_DIR/.."
cd "$PROJECT_ROOT" || exit 1

PID_FILE="./app.pid"

if [ ! -f "$PID_FILE" ];then
    echo "app.pid 不存在，尝试按程序名杀进程"
    PID=$(pgrep -f "target/release/xau")
    if [ -z "$PID" ];then
        echo "未找到运行中的进程"
        exit 0
    fi
else
    PID=$(cat "$PID_FILE")
fi

echo "停止 pid: $PID"
kill "$PID"

for i in {1..10};do
    if ! ps -p "$PID" >/dev/null;then
        break
    fi
    sleep 0.5
done
if ps -p "$PID" >/dev/null;then
    echo "进程未退出，强制 kill -9"
    kill -9 "$PID"
fi

rm -f "$PID_FILE"
echo "已停止"
