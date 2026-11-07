#!/bin/sh
# 监听 3478 / 45000，统计从外网到达的 UDP 包（7801 被 lrc-relay 占着，不测它）
pkill -f 'udp-multi-listen' 2>/dev/null || true
: > /tmp/multi.log
setsid nohup python3 -c "
import socket, select, time
ports = [3478, 45000]
socks = []
for p in ports:
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    s.bind(('0.0.0.0', p))
    socks.append(s)
print('listening on 3478,45000', flush=True)
counts = {p: 0 for p in ports}
deadline = time.time() + 20
while time.time() < deadline:
    r, _, _ = select.select(socks, [], [], 0.5)
    for s in r:
        data, addr = s.recvfrom(2048)
        port = s.getsockname()[1]
        counts[port] += 1
        print('port=%d from=%s len=%d' % (port, addr, len(data)), flush=True)
print('SUMMARY ' + ' '.join('%d=%d' % (p, c) for p, c in counts.items()), flush=True)
" udp-multi-listen > /tmp/multi.log 2>&1 < /dev/null &
sleep 2
cat /tmp/multi.log
