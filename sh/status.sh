#!/bin/bash
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

echo "==== $APP status ===="
pids=$(running_pids)
if [ -n "$pids" ]; then
    echo "✅ 正在运行，pid: $(echo "$pids" | tr '\n' ' ')"
    # ps 拿不到详情就算了（有些精简镜像的 ps 不支持这些字段），上面那行已经够用
    ps -o pid,pcpu,rss,etime,args -p "$(echo "$pids" | paste -sd, -)" 2>/dev/null |
        awk 'NR==1{print "  " $0; next} {$3=int($3/1024)"M"; print "  " $0}' || true
else
    echo "❌ 未运行"
fi

echo
if [ -f "$LOG" ]; then
    echo "---- $LOG 最后 15 行（共 $(wc -l <"$LOG") 行，$(du -h "$LOG" | cut -f1)）----"
    tail -n 15 "$LOG"
else
    echo "---- 还没有日志文件 $LOG ----"
fi
