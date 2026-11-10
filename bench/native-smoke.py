"""Exercise the native viewer between two private Aizome instances.

No session or host application is used. Evidence is retained at original resolution.
Python: websockets, Pillow. PATH: aizome, aizomectl, grim.
"""
import asyncio
import io
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import time
import urllib.request

from PIL import Image
import websockets

ROOT = Path(__file__).resolve().parents[1]
EVIDENCE = Path(tempfile.mkdtemp(prefix="voxl-native-"))
children = []
base_env = os.environ.copy()
for key in ("DISPLAY", "WAYLAND_SOCKET", "AIZOME_SOCKET", "WAYLAND_DISPLAY", "AIZOME_LOGIC"):
    base_env.pop(key, None)
for key, name in [("XDG_RUNTIME_DIR", "runtime"), ("XDG_CONFIG_HOME", "config"), ("XDG_STATE_HOME", "state"),
                  ("XDG_DATA_HOME", "data"), ("XDG_CACHE_HOME", "cache"), ("HOME", "home")]:
    directory = EVIDENCE / name
    directory.mkdir(mode=0o700)
    base_env[key] = str(directory)
base_env["VOXL_PASSWORD"] = "isolated-native-viewer"
base_env["RUST_LOG"] = "info"
base_env["SDL_VIDEODRIVER"] = "wayland"


def environment(name):
    return dict(base_env, WAYLAND_DISPLAY=name,
                AIZOME_SOCKET=str(EVIDENCE / f"runtime/aizome.{name}.sock"))


def start(name, args, env):
    package_libs=os.environ.get("VOXL_PACKAGE_LIBRARY_PATH")
    if package_libs is not None and args[0] in (os.environ.get("VOXL_BIN"),os.environ.get("VOXL_VIEWER")):
        env=dict(env,LD_LIBRARY_PATH=package_libs)
    with open(EVIDENCE / (name + ".log"), "wb") as log:
        p = subprocess.Popen(args, env=env, stdout=log, stderr=subprocess.STDOUT)
    children.append(p)
    return p


def ipc(env, request):
    with socket.socket(socket.AF_UNIX) as s:
        s.settimeout(3)
        s.connect(env["AIZOME_SOCKET"])
        s.sendall(json.dumps(request).encode() + b"\n")
        result = json.loads(s.makefile("rb").readline())
        assert result["response"] != "error", result
        return result


async def until(test, description, timeout=15):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        try:
            result = test()
            if result:
                return result
        except (OSError, ValueError):
            pass
        await asyncio.sleep(.05)
    raise AssertionError(description)


async def main():
    source, receiver = environment("source"), environment("receiver")
    for name, env in [("source", source), ("receiver", receiver)]:
        start(name, [os.environ.get("AIZOME_BIN", "aizome"), "--backend", "headless", "--renderer", "gles",
                     "--socket", name, "--no-session", "--no-shell", "--no-xwayland",
                     "--headless-output", "TEST=1280x800@1"], env)
        await until(lambda: ipc(env, {"request": "version"}), f"{name} compositor did not start")
    start("probe", [str(ROOT / "target/debug/examples/wayland-probe")], source)
    probe = await until(lambda: next((w for w in ipc(source, {"request":"windows"})["windows"]
                                     if w["app_id"] == "voxl-wayland-probe"), None), "probe absent")
    ipc(source, {"request":"move", "id":probe["id"], "x":100, "y":100})
    ipc(source, {"request":"focus", "id":probe["id"]})
    with socket.socket() as sock:
        sock.bind(("127.0.0.1",0)); port=sock.getsockname()[1]
    server_command = [os.environ.get("VOXL_BIN", str(ROOT/"result/bin/voxl")),
        "--capture-backend=wayland", "--video-backend="+os.environ.get("VOXL_VIDEO", "cpu"),
        "--stripes="+os.environ.get("VOXL_STRIPES","1"),
        f"--port={port}", "--audio-enabled=false", "--file-transfers=none"]
    server = start("server", server_command, source)
    await until(lambda: b"listening on" in (EVIDENCE/"server.log").read_bytes(), "server absent")
    viewer = start("viewer", [os.environ.get("VOXL_VIEWER",str(ROOT/"native/target/debug/voxl-viewer")),
        f"http://127.0.0.1:{port}/?token=isolated-native-viewer", "--decoder="+os.environ.get("VOXL_DECODE","cpu")], receiver)
    window = await until(lambda: next((w for w in ipc(receiver,{"request":"windows"})["windows"]
                                      if "Voxl" in w.get("title", "")), None), "viewer absent")
    ipc(receiver,{"request":"focus","id":window["id"]})
    await asyncio.sleep(.5)
    # Read the receiver compositor's actual presented output, not the decoder's buffer.
    def shot(name):
        output = EVIDENCE/(name+".png")
        result = subprocess.run(["grim", "-o", "TEST", str(output)],env=receiver,capture_output=True,timeout=5)
        assert result.returncode==0,result.stderr.decode()
        return Image.open(output).convert("RGB")
    def colored(name, green):
        image=shot(name)
        pixels=[]
        for y in range(0,image.height,8):
            for x in range(0,image.width,8):
                r,g,b=image.getpixel((x,y))
                if (g>140 and g>b*2 and g>r*2) if green else (b>140 and b>g*2 and b>r*2):
                    pixels.append((x,y))
        return pixels
    blue=await until(lambda: colored("viewer-blue",False),"native surface pixels absent from receiver")
    x=sum(p[0] for p in blue)//len(blue);y=sum(p[1] for p in blue)//len(blue)
    ipc(receiver,{"request":"synthesize","events":[
        {"type":"pointer-motion","x":x,"y":y},
        {"type":"pointer-button","button":"272","pressed":True},
        {"type":"pointer-button","button":"272","pressed":False},
        {"type":"key","key":"57","pressed":True},
        {"type":"key","key":"57","pressed":False}]})
    await until(lambda: colored("viewer-green",True),"native viewer input did not change sender's pixels")
    assert viewer.poll() is None, (EVIDENCE/"viewer.log").read_text()
    checks=["native Wayland viewer", "H.264 displayed on receiving Aizome", "pointer and keyboard return path"]
    if os.environ.get("VOXL_EXTENDED"):
        # A normal printable key must stay down, rather than becoming an IME tap.
        ipc(receiver,{"request":"synthesize","events":[{"type":"key","key":"30","pressed":True}]})
        await until(lambda:ipc(source,{"request":"input-state"})["state"]["pressed_keys"],"printable key was not held")
        await asyncio.sleep(.1)
        assert ipc(source,{"request":"input-state"})["state"]["pressed_keys"],"printable key released prematurely"
        ipc(receiver,{"request":"synthesize","events":[{"type":"key","key":"30","pressed":False}]})
        await until(lambda:not ipc(source,{"request":"input-state"})["state"]["pressed_keys"],"printable key remained held")
        # Focus loss must release remote modifiers even without key-up delivery.
        ipc(receiver,{"request":"synthesize","events":[{"type":"key","key":"29","pressed":True}]})
        await until(lambda:ipc(source,{"request":"input-state"})["state"]["pressed_keys"],"modifier was not held")
        local_probe=start("local-probe",[str(ROOT/"target/debug/examples/wayland-probe")],receiver)
        local_window=await until(lambda:next((w for w in ipc(receiver,{"request":"windows"})["windows"]
            if w["app_id"]=="voxl-wayland-probe"),None),"local focus target absent")
        ipc(receiver,{"request":"focus","id":local_window["id"]})
        await until(lambda:not ipc(source,{"request":"input-state"})["state"]["pressed_keys"],"focus loss did not release remote input")
        ipc(receiver,{"request":"synthesize","events":[{"type":"key","key":"29","pressed":False}]})
        local_probe.terminate();local_probe.wait(timeout=5)
        ipc(receiver,{"request":"focus","id":window["id"]})
        ipc(source,{"request":"output-config","name":"TEST","mode":"1024x768"})
        await until(lambda:"stream 1024x768" in (EVIDENCE/"viewer.log").read_text(),"native viewer did not resize")
        # This fixture repaints blue when the compositor sends a new configure.
        await until(lambda:colored("viewer-resized",False),"resized pixels absent")
        ipc(source,{"request":"output-config","name":"TEST","transform":"90"})
        await until(lambda:"stream 768x1024" in (EVIDENCE/"viewer.log").read_text(),"native viewer did not rotate")
        await until(lambda:colored("viewer-rotated",False),"rotated pixels absent")
        server.terminate();server.wait(timeout=5)
        await until(lambda:"Disconnected:" in (EVIDENCE/"viewer.log").read_text(),"viewer did not notice disconnect")
        start("server-restarted",server_command,source)
        await until(lambda:"client 1 connected" in (EVIDENCE/"server-restarted.log").read_text(),"viewer did not reconnect")
        await until(lambda:colored("viewer-reconnected",False),"reconnected pixels absent")
        assert viewer.poll() is None,(EVIDENCE/"viewer.log").read_text()
        checks.extend(["printable key lifetime","focus-loss release","native viewer resize/rotation","reconnection"])
    # Closing a native viewer releases input at the remote server.
    viewer.terminate();viewer.wait(timeout=5)
    await until(lambda: not ipc(source,{"request":"input-state"})["state"]["pressed_keys"],"held remote key")
    report={"checks":checks,
            "decoder":os.environ.get("VOXL_DECODE","cpu"), "source":ipc(source,{"request":"version"}),
            "receiver":ipc(receiver,{"request":"version"}), "evidence":str(EVIDENCE)}
    (EVIDENCE/"result.json").write_text(json.dumps(report,indent=2)+"\n")
    print(json.dumps(report,indent=2))


if __name__ == "__main__":
    try:
        asyncio.run(main())
    finally:
        for p in reversed(children):
            if p.poll() is None:
                p.terminate()
                try:p.wait(timeout=3)
                except subprocess.TimeoutExpired:p.kill();p.wait()
        print("Evidence:",EVIDENCE)
