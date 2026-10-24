#!/usr/bin/env python3
"""UDP relay on 127.0.0.1 that imitates a WAN path for tilt's stream (tools/wan/run.sh): one-way
delay, jitter, random loss and a rate-limited bottleneck queue in each direction. Packets keep
their order (jitter never reorders), as on most real paths.

  relay.py LISTEN_PORT TARGET_PORT [--delay MS] [--jitter MS] [--loss FRACTION] [--mbps N]
"""
import argparse, asyncio, random, socket, time

p = argparse.ArgumentParser()
p.add_argument("listen", type=int)
p.add_argument("target", type=int)
p.add_argument("--delay", type=float, default=25.0, help="one-way ms")
p.add_argument("--jitter", type=float, default=0.0, help="ms, uniform 0..jitter added")
p.add_argument("--loss", type=float, default=0.0, help="fraction")
p.add_argument("--mbps", type=float, default=0.0, help="bottleneck rate, 0 = none")
p.add_argument("--queue-ms", type=float, default=150.0)
a = p.parse_args()


class Dir:
    def __init__(self, send):
        self.send = send
        self.link_free = 0.0
        self.last_out = 0.0

    def push(self, data):
        now = time.monotonic()
        if a.loss and random.random() < a.loss:
            return
        t = now
        if a.mbps:
            start = max(now, self.link_free)
            if start - now > a.queue_ms / 1000:
                return  # tail drop
            self.link_free = start + len(data) * 8 / (a.mbps * 1e6)
            t = self.link_free
        t += a.delay / 1000 + random.random() * a.jitter / 1000
        t = max(t, self.last_out)
        self.last_out = t
        asyncio.get_running_loop().call_later(max(0, t - now), self.send, data)


async def main():
    loop = asyncio.get_running_loop()
    lsock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    lsock.bind(("127.0.0.1", a.listen))
    lsock.setblocking(False)
    clients = {}

    def from_client():
        while True:
            try:
                data, addr = lsock.recvfrom(65536)
            except BlockingIOError:
                return
            c = clients.get(addr)
            if c is None:
                up = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
                up.connect(("127.0.0.1", a.target))
                up.setblocking(False)
                down = Dir(lambda d, addr=addr: lsock.sendto(d, addr))
                upd = Dir(lambda d, up=up: up.send(d))
                c = clients[addr] = (up, upd, down)

                def from_server(up=up, down=down):
                    while True:
                        try:
                            d = up.recv(65536)
                        except BlockingIOError:
                            return
                        down.push(d)

                loop.add_reader(up.fileno(), from_server)
            c[1].push(data)

    loop.add_reader(lsock.fileno(), from_client)
    await asyncio.Event().wait()


asyncio.run(main())
