#!/bin/bash
# keep_alive.sh — Oracle ARM 防闲置
#
# 提醒：这个脚本对 Oracle 的回收判定基本没用。回收条件是
# CPU / 网络 / 内存三项 95 分位连续 7 天都 <20%（见 server.sh），
# 而每 10 分钟跑几万次 bash 乘法 + 一次 curl，95 分位约等于 0%。
# 真正管用的是把内存占到 40%（server.sh 里检查的那个 mem-hold 服务）。
# 这里留着只当「机器还活着」的心跳记录。

set -uo pipefail

LOG_FILE="/tmp/keep_alive.log"

# /tmp 可能被系统清理，也别让它无限涨：超过 1000 行就只留最后 500 行
if [ -f "$LOG_FILE" ] && [ "$(wc -l <"$LOG_FILE")" -gt 1000 ]; then
    tail -n 500 "$LOG_FILE" >"$LOG_FILE.tmp" && mv "$LOG_FILE.tmp" "$LOG_FILE"
fi

echo "$(date '+%F %T') keep-alive run" >>"$LOG_FILE"

# 一点点 CPU
for i in {1..15000}; do
    a=$((i * i))
done

# 一点点出站流量
curl -s --max-time 3 https://ifconfig.me >/dev/null 2>&1 || true
