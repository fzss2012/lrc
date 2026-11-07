#!/bin/sh
# 控制实验：VPS 自己给自己发 UDP 7801，验证 tcpdump 抓包本身是否有效
: > /tmp/cap2.txt
sudo pkill tcpdump 2>/dev/null || true
sleep 1
sudo setsid nohup tcpdump -n -U -l -i any udp port 7801 > /tmp/cap2.txt 2>&1 < /dev/null &
sleep 2
echo "=== 本机自发自收 3 个包 ==="
python3 -c "
import socket
s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
for i in range(3):
    s.sendto(b'loopback-control', ('127.0.0.1', 7801))
print('sent 3')
"
sleep 2
echo "=== 抓包结果 ==="
cat /tmp/cap2.txt
echo "=== 包数 ==="
grep -c "IP" /tmp/cap2.txt || echo 0
sudo pkill tcpdump 2>/dev/null || true
