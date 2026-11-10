"""Native viewer fault, fallback, and bounded audio tests on a private Aizome.

The fixture server sends the real wire format and malformed variants. SDL's
dummy audio driver consumes playback without using the login session's devices.
"""
import asyncio
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import zlib

from websockets.asyncio.server import serve

ROOT = Path(__file__).resolve().parents[1]
EVIDENCE = Path(tempfile.mkdtemp(prefix="voxl-native-faults-"))
children = []
env = os.environ.copy()
for key in ("DISPLAY", "WAYLAND_SOCKET", "AIZOME_SOCKET", "WAYLAND_DISPLAY", "AIZOME_LOGIC", "VOXL_TOKEN"):
    env.pop(key, None)
for key in ("XDG_RUNTIME_DIR", "XDG_CONFIG_HOME", "XDG_STATE_HOME", "XDG_DATA_HOME", "XDG_CACHE_HOME", "HOME"):
    directory = EVIDENCE / key.lower()
    directory.mkdir(mode=0o700)
    env[key] = str(directory)
env.update(WAYLAND_DISPLAY="faults", AIZOME_SOCKET=f"{env['XDG_RUNTIME_DIR']}/aizome.faults.sock",
           SDL_VIDEODRIVER="wayland", SDL_AUDIODRIVER="dummy", RUST_LOG="info")


def start(name, command, environment):
    with (EVIDENCE / (name + ".log")).open("wb") as output:
        process = subprocess.Popen(command, env=environment, stdout=output, stderr=subprocess.STDOUT)
    children.append(process)
    return process


def ipc(request):
    with socket.socket(socket.AF_UNIX) as connection:
        connection.settimeout(2)
        connection.connect(env["AIZOME_SOCKET"])
        connection.sendall(json.dumps(request).encode() + b"\n")
        return json.loads(connection.makefile("rb").readline())


async def until(test, reason, timeout=15):
    end = asyncio.get_running_loop().time() + timeout
    while asyncio.get_running_loop().time() < end:
        try:
            result = test()
            if result:
                return result
        except (OSError, ValueError):
            pass
        await asyncio.sleep(.02)
    raise AssertionError(reason)


def packet(bitstream, ident=1, *, parts=1, stripes=1, key=True):
    header = bytearray(16)
    header[0] = 1
    header[1] = int(key)
    header[2:6] = ident.to_bytes(4, "big")
    header[11] = stripes
    header[12] = parts
    return bytes(header) + bitstream


async def main():
    start("compositor", [os.environ.get("AIZOME_BIN", "aizome"), "--backend", "headless",
        "--renderer", "gles", "--socket", "faults", "--no-session", "--no-shell", "--no-xwayland",
        "--headless-output", "TEST=640x480@1"], env)
    await until(lambda: ipc({"request": "version"}), "private compositor unavailable")
    encoded = subprocess.run(["ffmpeg", "-hide_banner", "-loglevel", "error", "-f", "lavfi", "-i",
        "color=c=0x3366cc:s=320x240:r=60", "-frames:v", "1", "-c:v", "libx264", "-preset", "ultrafast",
        "-tune", "zerolatency", "-crf", "18", "-f", "h264", "-"], capture_output=True, check=True).stdout
    (EVIDENCE / "fixture.h264").write_bytes(encoded)
    hello = {"t": "hello", "w": 320, "h": 240, "stripes": [[0, 240]], "audio": False}
    results = []

    async def run_case(name, messages, *, mode="cpu", expected=None, environment=None, audio=False, deny=False, widgets=False, scene_frames=None, fallback=False):
        received = []
        sent = asyncio.Event()
        async def handle(ws):
            try:
                for message in messages:
                    await ws.send(json.dumps(message) if isinstance(message, dict) else message)
                sent.set()
                async for message in ws:
                    received.append(message)
            except Exception:
                sent.set()
        def reject(connection, request):
            return connection.respond(401, "test authentication required\n") if deny else None
        async with serve(handle, "127.0.0.1", 0, process_request=reject) as server:
            port = server.sockets[0].getsockname()[1]
            viewer = start(name, [os.environ.get("VOXL_VIEWER", str(ROOT / "native/target/release/voxl-viewer")),
                f"http://127.0.0.1:{port}/", f"--decoder={mode}", "--fullscreen", "--seconds=3"] + (["--native-widgets"] if widgets else []),
                dict(env, **(environment or {})))
            await until(lambda: viewer.poll() is not None, f"{name}: viewer hung", 12)
        log = (EVIDENCE / (name + ".log")).read_text()
        assert "panicked at" not in log, log
        if expected:
            assert viewer.returncode == 1 and expected in log, (name, viewer.returncode, log)
            metrics = None
        else:
            assert viewer.returncode == 0, (name, log)
            metrics = next(json.loads(line) for line in reversed(log.splitlines()) if line.startswith("{"))
            assert metrics["frames"] >= 1, (name, metrics)
            assert "a,1" in received, received
            if mode == "auto" and environment:
                assert not metrics["hardware_decode"], metrics
            if audio:
                assert "au,1" in received, received
                assert metrics["audio_packets"] == 400, metrics
                assert metrics["audio_discarded_queues"] > 0, metrics
        if scene_frames is not None:
            assert metrics["scene_frames"]==scene_frames,(name,metrics,log)
        if fallback:
            assert "scene,0,2" in received and "v,1" in received,(name,received,log)
        results.append({"case": name, "exit": viewer.returncode, "metrics": metrics})
        print(name, "passed", flush=True)

    await run_case("cpu-valid", [hello, packet(encoded)])
    await run_case("auto-no-cuda", [hello, packet(encoded)], mode="auto", environment={"CUDA_VISIBLE_DEVICES": "-1"})
    await run_case("strict-cuda-unavailable", [hello], mode="cuda", environment={"CUDA_VISIBLE_DEVICES": "-1"}, expected="CUDA decoder device")
    await run_case("invalid-geometry", [dict(hello, w=0xffffffff)], expected="invalid stream dimensions")
    await run_case("truncated-packet", [hello, b"\x01"], expected="invalid video packet length/type")
    await run_case("decoded-geometry-mismatch", [dict(hello, w=322), packet(encoded)], expected="decoded dimensions differ")
    await run_case("invalid-json", ["{"], expected="EOF while parsing")
    await run_case("unauthorized", [], deny=True, expected="HTTP 401")
    striped = dict(hello, stripes=[[0, 120], [120, 120]])
    partial = packet(encoded, parts=2, stripes=2, key=False)
    await run_case("duplicate-stripe", [striped, partial, partial], expected="inconsistent or repeated video stripe")
    await run_case("bounded-incomplete-frames", [striped] +
        [packet(encoded, i, parts=2, stripes=2, key=False) for i in range(1, 10)], expected="too many incomplete video frames")
    await run_case("bounded-audio", [dict(hello, audio=True), packet(encoded)] + [b"\x02\xf8\xff\xfe"] * 400, audio=True)
    if os.environ.get("AIZOME_REMOTE_RENDERER"):
        frame={"version":1,"serial":1,"width":320,"height":240,"scale":1.0,"full":True,"fonts":[],"images":[],
            "batches":[{"damage":[[0,0,320,240]],"ops":[{"Fill":{"rect":[0,0,320,240],"color":[0.2,0.4,0.7,1],"blend":1}}]}]}
        def scene_packet(frame, serial=1, nonce=1):
            return bytes([0x20])+nonce.to_bytes(4,"little")+serial.to_bytes(4,"little")+json.dumps(frame).encode()
        def compressed_frames(frames,nonce=1):
            codec=zlib.compressobj(1)
            messages=[]
            for sequence,frame in enumerate(frames,1):
                raw=json.dumps(frame).encode()
                compressed=codec.compress(raw)+codec.flush(zlib.Z_SYNC_FLUSH)
                messages.append(bytes([0x21])+nonce.to_bytes(4,"little")+sequence.to_bytes(4,"little")+len(raw).to_bytes(4,"little")+compressed)
            return messages
        await run_case("scene-valid",[hello,packet(encoded),scene_packet(frame)],widgets=True,scene_frames=1)
        await run_case("scene-truncated",[hello,b"\x20"],widgets=True,expected="truncated retained packet")
        await run_case("scene-invalid-version",[hello,packet(encoded),scene_packet(dict(frame,version=99))],widgets=True,scene_frames=0,fallback=True)
        await run_case("scene-delta-without-full",[hello,packet(encoded),scene_packet(dict(frame,full=False))],widgets=True,scene_frames=0,fallback=True)
        await run_case("scene-dimensions",[hello,packet(encoded),scene_packet(dict(frame,width=0xffffffff))],widgets=True,scene_frames=0,fallback=True)
        await run_case("scene-stale-epoch",[hello,packet(encoded),scene_packet(frame,nonce=99)],widgets=True,scene_frames=0)
        await run_case("scene-gap-then-stale-full",[hello,packet(encoded),scene_packet(frame),
            scene_packet(dict(frame,serial=3,full=False),2),scene_packet(dict(frame,serial=4),3)],widgets=True,scene_frames=1,fallback=True)
        compressed=compressed_frames([frame,dict(frame,serial=2,full=False)])
        await run_case("scene-zlib-dictionary",[hello,packet(encoded)]+compressed,widgets=True,scene_frames=2)
        await run_case("scene-zlib-truncated",[hello,packet(encoded),compressed[0][:-1]],widgets=True,scene_frames=0,fallback=True)
        forged=bytearray(compressed[0]);forged[9:13]=(0xffffffff).to_bytes(4,"little")
        await run_case("scene-zlib-expansion-limit",[hello,packet(encoded),bytes(forged)],widgets=True,scene_frames=0,fallback=True)
        await run_case("scene-zlib-reconnect-epoch",[hello,packet(encoded),compressed[0],hello,packet(encoded)]+compressed_frames([frame],nonce=2),widgets=True,scene_frames=2)
    (EVIDENCE / "result.json").write_text(json.dumps(results, indent=2) + "\n")


if __name__ == "__main__":
    try:
        asyncio.run(main())
    finally:
        for process in reversed(children):
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()
        print("Evidence:", EVIDENCE, flush=True)
