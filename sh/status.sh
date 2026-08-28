#!/bin/bash
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

echo "==== $APP status ===="
pids=$(running_pids)
if [ -n "$pids" ]; then
    echo "✅ 正在运行，pid: $(echo "$pids" | tr '\n' ' ')"
    # 顺便报一下在跑的是哪个档 —— exe 路径里就有
    for p in $pids; do
        exe=$(readlink "/proc/$p/exe" 2>/dev/null)
        [ -n "$exe" ] && echo "     $p -> ${exe% (deleted)}"
    done
    # ps 拿不到详情就算了（有些精简镜像的 ps 不支持这些字段），上面那行已经够用
    ps -o pid,pcpu,rss,etime -p "$(echo "$pids" | paste -sd, -)" 2>/dev/null |
        awk 'NR==1{print "  " $0; next} {$3=int($3/1024)"M"; print "  " $0}' || true
else
    echo "❌ 未运行"
fi

echo
echo "---- 已编译的版本 ----"
for b in "${ALL_BINS[@]}"; do
    if [ -x "$b" ]; then
        echo "  $b   $(date -r "$b" '+%F %T' 2>/dev/null)"
    else
        echo "  $b   (没有)"
    fi
done

echo
if [ -f "$LOG" ]; then
    echo "---- $LOG 最后 15 行（共 $(wc -l <"$LOG") 行）----"
    tail -n 15 "$LOG"
else
    echo "---- 还没有日志文件 $LOG ----"
fi
