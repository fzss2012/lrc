import socket
s = socket.socket()
s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
s.bind(('0.0.0.0', 7800))
s.listen(5)
print('tcp-7800 ready', flush=True)
while True:
    c, a = s.accept()
    print('TCP accept from ' + str(a), flush=True)
    try:
        c.sendall(b'ok')
    except Exception:
        pass
    c.close()
