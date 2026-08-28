#!/bin/bash
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

pids=$(running_pids)
if [ -z "$pids" ]; then
    echo "$APP 未在运行"
    exit 0
fi

echo "停止 pid: $(echo "$pids" | tr '\n' ' ')"
# shellcheck disable=SC2086
kill -TERM $pids 2>/dev/null

# 必须等它真的退出：端口是进程被回收之后才释放的，
# 不等就立刻重启会撞 Address already in use
for _ in $(seq 1 20); do
    [ -z "$(running_pids)" ] && break
    sleep 0.25
done

left=$(running_pids)
if [ -n "$left" ]; then
    echo "5 秒还没退出，强制 kill -9: $(echo "$left" | tr '\n' ' ')"
    # shellcheck disable=SC2086
    kill -KILL $left 2>/dev/null
    sleep 0.3
fi

if [ -n "$(running_pids)" ]; then
    echo "⚠️ 仍有进程没杀掉: $(running_pids | tr '\n' ' ')"
    exit 1
fi
echo "已停止"
