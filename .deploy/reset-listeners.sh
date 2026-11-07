#!/bin/sh
# 清掉所有临时 python 监听，重新起一个干净的 TCP 7800 监听
pkill -f "python3 -c" 2>/dev/null || true
sudo pkill tcpdump 2>/dev/null || true
sleep 1
echo "=== 7800 占用情况 ==="
ss -ltnp | grep 7800 || echo "7800 空闲"
echo "=== 起新监听 ==="
setsid nohup python3 /tmp/tcp7800.py > /tmp/tcp7800.log 2>&1 < /dev/null &
sudo setsid nohup tcpdump -n -U -l -i any 'udp port 7801' > /tmp/cap5.txt 2>&1 < /dev/null &
sleep 2
cat /tmp/tcp7800.log
ss -ltnp | grep 7800
echo "relay: $(pgrep -c lrc-relay)  tcpdump: $(pgrep -c tcpdump)"
