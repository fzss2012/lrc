#!/bin/sh
# 起 tcpdump 抓 UDP 7801（-U 不缓冲、-l 行缓冲，方便脚本式读取）
sudo pkill tcpdump 2>/dev/null || true
sleep 1
: > /tmp/cap.txt
sudo setsid nohup tcpdump -n -U -l -i any udp port 7801 > /tmp/cap.txt 2>&1 < /dev/null &
sleep 2
echo "tcpdump started:"
pgrep -a tcpdump | head -2
