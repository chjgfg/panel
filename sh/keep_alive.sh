#!/bin/bash
# keep_alive.sh Oracle ARM防闲置小脚本
# 每10分钟做一点点CPU计算，输出一点网络日志，避免7天长期零负载

LOG_FILE="/tmp/keep_alive.log"
echo "$(date) keep‑alive run" >> $LOG_FILE
# 简单短计算，产生极少量CPU
for i in {1..15000}; do
  a=$((i*i))
done
# 访问一个轻量公网地址，产生出站网络流量
curl -s --max-time 3 https://ifconfig.me > /dev/null 2>&1
