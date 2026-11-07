#!/bin/sh
# 谨慎版 UDP 入站测试：tcpdump 抓 udp/7801 与 icmp，同时确认 relay 在听
sudo pkill tcpdump 2>/dev/null || true
sleep 1
: > /tmp/cap3.txt
sudo setsid nohup tcpdump -n -U -l -i any '(udp port 7801) or icmp' > /tmp/cap3.txt 2>&1 < /dev/null &
sleep 2
echo "relay: $(pgrep -c lrc-relay) 个进程在跑"
echo "listening: $(ss -lun | grep -c 7801) 条 7801 监听"
echo "tcpdump: $(pgrep -c tcpdump) 个"
