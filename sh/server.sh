#!/bin/bash
# server.sh — OCI A1 闲置回收自查
# 用法: ./server.sh        查看报告
#       ./server.sh --log  记录一次采样（给 systemd timer 用）

IFACE=enp0s6
BW_MBPS=1000                 # A1: 每 OCPU 1 Gbps，按实际 OCPU 数改
LOG=/var/log/oci-idle.csv

# ---------- 采样（CPU / 网络都需要 1 秒做差）----------
cpu_snap() { awk '/^cpu /{print $2+$3+$4+$5+$6+$7+$8+$9, $5}' /proc/stat; }
net_snap() { sed 's/:/ /' /proc/net/dev | awk -v d="$IFACE" '$1==d{print $2, $10}'; }

read -r T1 I1 <<<"$(cpu_snap)"; read -r RX1 TX1 <<<"$(net_snap)"
sleep 1
read -r T2 I2 <<<"$(cpu_snap)"; read -r RX2 TX2 <<<"$(net_snap)"

CPU_PCT=$(awk -v t=$((T2-T1)) -v i=$((I2-I1)) \
'BEGIN{ if(t>0) printf "%.2f", 100*(t-i)/t; else printf "0.00" }')

read -r M_TOTAL M_USED M_AVAIL <<<"$(free | awk '/^Mem:/{print $2, $3, $7}')"
MEM_PCT=$(awk -v a="$M_AVAIL" -v t="$M_TOTAL" 'BEGIN{printf "%.2f", (t-a)*100/t}')
MEM_USED_PCT=$(awk -v u="$M_USED" -v t="$M_TOTAL" 'BEGIN{printf "%.2f", u*100/t}')

NET_MBPS=$(awk -v r=$((RX2-RX1)) -v x=$((TX2-TX1)) 'BEGIN{printf "%.3f", (r+x)*8/1000000}')
NET_PCT=$(awk -v m="$NET_MBPS" -v b="$BW_MBPS" 'BEGIN{printf "%.3f", m*100/b}')

# ---------- --log 模式：只追加一行就退出 ----------
if [ "$1" = "--log" ]; then
    echo "$(date +%s),$CPU_PCT,$MEM_PCT,$NET_PCT" >> "$LOG"
    exit 0
fi

echo "===== 系统时间 ====="; date

echo -e "\n===== 内存 ====="; free -h
echo "内存利用率: ${MEM_PCT}% (available 口径, Oracle 用这个) / ${MEM_USED_PCT}% (used 口径)   阈值 <20%"

echo -e "\n===== 磁盘根分区 ====="; df -h /

echo -e "\n===== CPU ====="; uptime
echo "CPU 瞬时使用率: ${CPU_PCT}%   阈值 <20%（注意 1 核: load 1.00 = 满载）"

echo -e "\n===== 网络 $IFACE ====="
echo "瞬时吞吐: ${NET_MBPS} Mbps (rx+tx)   利用率: ${NET_PCT}% / ${BW_MBPS} Mbps   阈值 <20%"
sed 's/:/ /' /proc/net/dev | awk -v d="$IFACE" \
'$1==d{printf "累计: rx %.2f GiB, tx %.2f GiB\n", $2/1073741824, $10/1073741824}'

echo -e "\n===== xau 进程 ====="; pgrep -a xau || echo "未运行"

echo -e "\n===== 守护进程 ====="
for s in oracle-cloud-agent mem-hold; do
    if systemctl is-active --quiet "$s"; then
        echo "$s: active"
    else
        echo "$s: ⚠️ 未运行"
    fi
done

# ---------- 本地 7 天 95 分位 ----------
echo -e "\n===== 本地 7 天 95 分位（自建采样）====="
if [ -r "$LOG" ]; then
    CUT=$(( $(date +%s) - 7*86400 ))
    p95() {
        awk -F, -v cut="$CUT" -v c="$1" '$1>=cut{print $c}' "$LOG" | sort -g | \
        awk '{v[NR]=$1} END{ if(NR==0){printf "n/a"; exit}
            k=int(NR*0.95); if(k<1)k=1; printf "%.2f", v[k] }'
    }
    N=$(awk -F, -v cut="$CUT" '$1>=cut' "$LOG" | wc -l)
    echo "样本 $N 条    CPU $(p95 2)%   内存 $(p95 3)%   网络 $(p95 4)%"
    [ "$N" -lt 1000 ] && echo "（样本不足，建议攒满 7 天 ≈ 10080 条再看）"
else
    echo "$LOG 不存在，先配好下面的 timer"
fi

echo -e "\n===== Oracle A1 回收条件（三条同时满足连续 7 天）====="
echo "1. CPU 95 分位 <20%    2. 网络利用率 <20%    3. 内存利用率 <20%（仅 A1 机型）"
echo "破任意一条即可 → 占内存到 40% 是成本最低的做法"