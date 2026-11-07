#!/bin/sh
# 同一时刻对照：TCP 7800 可达 vs UDP 7801 是否到达
pkill -f 'tcp-7800-listen' 2>/dev/null || true
sudo pkill tcpdump 2>/dev/null || true
sleep 1
: > /tmp/cap4.txt
setsid nohup python3 -c "
import socket
s = socket.socket(); s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
s.bind(('0.0.0.0', 7800)); s.listen(5)
print('tcp-7800 ready', flush=True)
while True:
    c, a = s.accept()
    print('TCP accept from ' + str(a), flush=True)
    c.sendall(b'ok'); c.close()
" tcp-7800-listen > /tmp/tcp7800.log 2>&1 < /dev/null &
sudo setsid nohup tcpdump -n -U -l -i any 'udp port 7801' > /tmp/cap4.txt 2>&1 < /dev/null &
sleep 2
cat /tmp/tcp7800.log
echo "udp-listening: $(ss -lun | grep -c 7801)"
