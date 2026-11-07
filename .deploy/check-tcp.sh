#!/bin/sh
# 在 VPS 上起一个临时 TCP 监听（7800），用于判断安全组是否只挡了 UDP
pkill -f 'nc -l' 2>/dev/null || true
echo "=== ufw ==="
sudo ufw status 2>/dev/null | head -5 || echo "ufw 未安装/未启用"
echo "=== iptables INPUT ==="
sudo iptables -S INPUT 2>/dev/null | head -10
echo "=== 起临时 TCP 7800 监听 ==="
setsid nohup python3 -c "
import socket
s = socket.socket(); s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
s.bind(('0.0.0.0', 7800)); s.listen(5)
print('tcp 7800 listening', flush=True)
while True:
    c, a = s.accept()
    print('got connection from', a, flush=True)
    c.sendall(b'lrc-hello')
    c.close()
" > /tmp/tcp7800.log 2>&1 < /dev/null &
sleep 2
cat /tmp/tcp7800.log
ss -ltnp | grep 7800 || echo "no tcp 7800"
