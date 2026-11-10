"""Private TCP link emulator; no host routes, qdiscs, devices or privileges.

Each direction has a bounded FIFO, fixed propagation delay and optional serial
byte rate. This models an ordered reliable link, not packet loss or a real WAN's
TCP congestion control. It is deliberately outside the latency observer.
"""
import asyncio
import contextlib
import math


class Link:
    def __init__(self, upstream_port, mbps=0, delay_ms=0):
        if not all(math.isfinite(v) and v >= 0 for v in (mbps, delay_ms)):
            raise ValueError("link rate and delay must be finite and nonnegative")
        self.upstream_port = upstream_port
        self.rate = mbps * 1_000_000 / 8
        self.delay = delay_ms / 1000
        self.server = None
        self.tasks = set()
        self.stats = {d: {"bytes": 0, "peak_queued_bytes": 0} for d in ("up", "down")}

    async def start(self):
        self.server = await asyncio.start_server(self._connection, "127.0.0.1", 0, limit=65536)
        return self.server.sockets[0].getsockname()[1]

    async def close(self):
        if self.server is not None:
            self.server.close()
        for task in list(self.tasks):
            task.cancel()
        await asyncio.gather(*list(self.tasks), return_exceptions=True)
        # Python 3.12+ also waits for active transports here. Cancel and close
        # those connections first, including a viewer intentionally kept open.
        if self.server is not None:
            await self.server.wait_closed()

    async def _direction(self, reader, writer, direction):
        queue = asyncio.Queue(maxsize=32)
        loop = asyncio.get_running_loop()
        queued = 0
        stats = self.stats[direction]

        async def receive():
            nonlocal queued
            while True:
                try:
                    data = await reader.read(16384)
                except OSError:
                    data = b""
                ready = loop.time() + self.delay
                await queue.put((ready, data))
                queued += len(data)
                stats["peak_queued_bytes"] = max(stats["peak_queued_bytes"], queued)
                if not data:
                    return

        receiver = asyncio.create_task(receive())
        finish = loop.time()
        try:
            while True:
                ready, data = await queue.get()
                queued -= len(data)
                if not data:
                    if writer.can_write_eof():
                        writer.write_eof()
                    return
                # MTU-sized scheduling avoids delivering a whole large read as
                # one artificial packet at the end of its serialization time.
                for offset in range(0, len(data), 1460):
                    packet = data[offset:offset + 1460]
                    finish = max(finish, ready) + (len(packet) / self.rate if self.rate else 0)
                    await asyncio.sleep(max(0, finish - loop.time()))
                    writer.write(packet)
                    await writer.drain()
                    stats["bytes"] += len(packet)
        finally:
            receiver.cancel()
            await asyncio.gather(receiver, return_exceptions=True)

    async def _connection(self, reader, writer):
        task = asyncio.current_task()
        self.tasks.add(task)
        upstream = None
        directions = []
        try:
            upstream_reader, upstream = await asyncio.open_connection("127.0.0.1", self.upstream_port, limit=65536)
            directions = [asyncio.create_task(self._direction(reader, upstream, "up")),
                          asyncio.create_task(self._direction(upstream_reader, writer, "down"))]
            await asyncio.gather(*directions)
        except (OSError, asyncio.IncompleteReadError):
            pass
        finally:
            for direction in directions:
                direction.cancel()
            await asyncio.gather(*directions, return_exceptions=True)
            for stream in (writer, upstream):
                if stream is not None:
                    stream.close()
                    with contextlib.suppress(OSError):
                        await stream.wait_closed()
            self.tasks.discard(task)


async def calibration():
    """Verify byte integrity, propagation and rate before trusting link scores."""
    payload = bytes(range(256)) * 256

    async def echo(reader, writer):
        data = await reader.readexactly(len(payload))
        writer.write(data)
        await writer.drain()
        writer.close()
        await writer.wait_closed()

    async with await asyncio.start_server(echo, "127.0.0.1", 0) as server:
        link = Link(server.sockets[0].getsockname()[1], mbps=1, delay_ms=25)
        port = await link.start()
        try:
            reader, writer = await asyncio.open_connection("127.0.0.1", port)
            start = asyncio.get_running_loop().time()
            writer.write(payload)
            await writer.drain()
            received = await asyncio.wait_for(reader.readexactly(len(payload)), 5)
            elapsed = asyncio.get_running_loop().time() - start
            expected = 2 * (len(payload) / 125000 + .025)
            assert received == payload
            assert expected - .01 <= elapsed < expected + .25, (elapsed, expected)
            writer.close()
            await writer.wait_closed()
            print({"calibration_seconds": elapsed, "expected_seconds": expected, "link": link.stats})
        finally:
            await link.close()

    async def hold(reader, writer):
        try:
            await reader.read()
        finally:
            writer.close()
            await writer.wait_closed()

    async with await asyncio.start_server(hold, "127.0.0.1", 0) as server:
        link = Link(server.sockets[0].getsockname()[1], mbps=5, delay_ms=20)
        reader, writer = await asyncio.open_connection("127.0.0.1", await link.start())
        await asyncio.sleep(.03)
        await asyncio.wait_for(link.close(), 1)
        assert await reader.read() == b""
        writer.close()
        await writer.wait_closed()
        print("Open-client teardown passed")


if __name__ == "__main__":
    asyncio.run(calibration())
