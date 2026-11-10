"""End-to-end native Wayland check, in a private headless Aizome with no Xwayland.

Build `cargo build --examples` and voxl first. Requires Python websockets, PyAV,
Pillow, plus wl-copy/wl-paste on PATH. AIZOME_BIN and VOXL_BIN override binaries.
Logs and unmodified PNGs are retained in the printed temporary directory.
"""
import asyncio
import io
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import tempfile
import time
import urllib.request

import av
from PIL import Image
import websockets

ROOT = Path(__file__).resolve().parents[1]
EVIDENCE = Path(tempfile.mkdtemp(prefix="voxl-aizome-"))
children = []
env = os.environ.copy()
for key in ("DISPLAY", "WAYLAND_SOCKET", "AIZOME_SOCKET", "WAYLAND_DISPLAY", "AIZOME_LOGIC"):
    env.pop(key, None)
for key, directory in [("XDG_RUNTIME_DIR", "runtime"), ("XDG_CONFIG_HOME", "config"),
                       ("XDG_STATE_HOME", "state"), ("XDG_DATA_HOME", "data"),
                       ("XDG_CACHE_HOME", "cache"), ("HOME", "home")]:
    path = EVIDENCE / directory
    path.mkdir(mode=0o700)
    env[key] = str(path)
env["WAYLAND_DISPLAY"] = "voxl-test"
env["AIZOME_SOCKET"] = str(EVIDENCE / "runtime/aizome.voxl-test.sock")
env["VOXL_PASSWORD"] = "isolated-native-test"
env["RUST_LOG"] = "info"


def start(name, args):
    log = open(EVIDENCE / (name + ".log"), "wb")
    p = subprocess.Popen(args, env=env, stdout=log, stderr=subprocess.STDOUT)
    log.close()
    children.append(p)
    return p


def ipc(request):
    with socket.socket(socket.AF_UNIX) as s:
        s.settimeout(3)
        s.connect(env["AIZOME_SOCKET"])
        s.sendall(json.dumps(request).encode() + b"\n")
        result = json.loads(s.makefile("rb").readline())
        assert result["response"] != "error", result
        return result


async def until(test, description, timeout=10):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        try:
            value = test()
            if value:
                return value
        except (OSError, ValueError):
            pass
        await asyncio.sleep(0.05)
    raise AssertionError(description)


async def main():
    start("aizome", [os.environ.get("AIZOME_BIN", "aizome"), "--backend", "headless",
          "--renderer", os.environ.get("AIZOME_TEST_RENDERER", "pixman"), "--socket", "voxl-test",
          "--no-session", "--no-shell", "--no-xwayland",
          "--headless-output", "FIRST=1280x800@1", "--headless-output", "SECOND=1280x800@2"])
    version = await until(lambda: ipc({"request": "version"}), "Aizome did not start")
    outputs = ipc({"request": "outputs"})["outputs"]
    output = next(o for o in outputs if o["name"] == "SECOND")
    start("probe", [str(ROOT / "target/debug/examples/wayland-probe")])
    def window():
        return next((w for w in ipc({"request": "windows"})["windows"] if w["app_id"] == "voxl-wayland-probe"), None)
    win = await until(window, "native window not mapped")
    rect = output["logical"]
    ipc({"request": "move", "id": win["id"], "x": rect["x"] + 40, "y": rect["y"] + 50})
    ipc({"request": "focus", "id": win["id"]})
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        port = s.getsockname()[1]
    server = start("voxl", [os.environ.get("VOXL_BIN", str(ROOT / "target/debug/voxl")),
                    "--capture-backend=wayland", "--wayland-output=SECOND",
                    f"--port={port}", "--audio-enabled=false", "--file-transfers=none"])
    base = f"http://127.0.0.1:{port}"

    def get(path):
        req = urllib.request.Request(base + path, headers={"Authorization": "Bearer isolated-native-test"})
        with urllib.request.urlopen(req, timeout=5) as r:
            return r.read()

    await until(lambda: get("/healthz"), "Voxl did not start")
    ws = await websockets.connect(f"ws://127.0.0.1:{port}/ws?token=isolated-native-test")
    hello = json.loads(await ws.recv())
    assert (hello["w"], hello["h"], hello["captureBackend"]) == (1280, 800, "wayland"), hello
    decoder = av.CodecContext.create("h264", "r")
    decoded = []
    clipboard = []
    geometries = [(hello["w"], hello["h"])]
    async def receive():
        nonlocal decoder
        async for message in ws:
            if isinstance(message, str):
                m = json.loads(message)
                if m.get("t") == "ping": await ws.send("po," + str(m["s"]))
                if m.get("t") == "clip": clipboard.append(m["d"])
                if m.get("t") == "hello":
                    geometries.append((m["w"], m["h"]))
                    decoder = av.CodecContext.create("h264", "r")
                continue
            if message[0] == 1:
                frames = decoder.decode(av.Packet(message[16:]))
                decoded.extend(frames)
                await ws.send("a," + str(int.from_bytes(message[2:6], "big")))
            elif message[0] == 4:
                clipboard.append(message[1:])
    reader = asyncio.create_task(receive())
    await ws.send("v,1")
    await until(lambda: decoded, "no decoded H.264 frames")

    # The native window is the blue rectangle in this selected output's pixels.
    shot = Image.open(io.BytesIO(get("/screenshot"))).convert("RGB")
    shot.save(EVIDENCE / "native-blue.png")
    blue = [(x, y) for y in range(0, shot.height, 8) for x in range(0, shot.width, 8)
            if shot.getpixel((x, y)) == (32, 64, 192)]
    assert blue, "native window absent from the capture"
    x = sum(p[0] for p in blue) // len(blue)
    y = sum(p[1] for p in blue) // len(blue)
    await ws.send(f"m,{x},{y}")
    await ws.send("b,1,1")
    await ws.send("b,1,0")
    await ws.send("sp,0,24")
    for sym in [ord("a"), ord("A"), 0x01000000 + ord("日")]:
        await ws.send(f"k,{sym},1")
        await ws.send(f"k,{sym},0")
    for command in ["k,65509,1", "k,65509,0", "k,65,1", "k,65,0",
                    "k,65505,1", "k,97,1", "k,97,0", "k,65505,0",
                    "k,65509,1", "k,65509,0", "k,97,1", "k,97,0"]:
        await ws.send(command)
    await ws.send("k,32,1")
    await ws.send("k,32,0")

    def green():
        return decoded and decoded[-1].to_image().getpixel((x, y))[1] > 150
    await until(green, "Space input did not return as green H.264 pixels")
    Image.open(io.BytesIO(get("/screenshot"))).save(EVIDENCE / "native-green.png")
    def input_observed():
        lines = (EVIDENCE / "probe.log").read_text().splitlines()
        events = [json.loads(l) for l in lines if l.startswith("{")]
        text = "".join(e.get("text", "") for e in events if e.get("down"))
        return "aA日Aaa " in text and any(e.get("button") == 272 for e in events) and any("scroll" in e for e in events)
    await until(input_observed, "native window did not receive text/button/scroll input")

    if os.environ.get("VOXL_BROWSER"):
        from playwright.async_api import async_playwright
        async with async_playwright() as playwright:
            browser = await playwright.chromium.launch(executable_path=os.environ["VOXL_BROWSER"], headless=True, env=env)
            page = await browser.new_page(viewport={"width":1280, "height":800}, device_scale_factor=1)
            errors = []
            page.on("pageerror", lambda error: errors.append(str(error)))
            await page.goto(base + "/?token=isolated-native-test")
            await page.wait_for_function("window.voxlStats && window.voxlStats.fps > 0")
            # This is the actual web viewer's WebCodecs/presentation output.
            browser_png = await page.screenshot(path=str(EVIDENCE / "browser.png"))
            browser_pixel = Image.open(io.BytesIO(browser_png)).convert("RGB").getpixel((x, y))
            assert browser_pixel[1] > 150 and browser_pixel[2] < 100, browser_pixel
            await page.mouse.click(x, y)
            await page.keyboard.press("b")
            await page.keyboard.insert_text("雪")
            await until(lambda: '"text":"雪"' in (EVIDENCE / "probe.log").read_text(), "browser Unicode input did not arrive")
            assert not errors, errors
            await browser.close()

    # Clipboard ownership reaches the compositor before the following input.
    await ws.send("c,voxl-wayland-clipboard")
    await asyncio.sleep(0.15)
    result = subprocess.check_output(["wl-paste", "--no-newline"], env=env, timeout=5)
    assert result == b"voxl-wayland-clipboard", result
    subprocess.run(["wl-copy", "from-native-wayland"], env=env, check=True, timeout=5)
    await until(lambda: "from-native-wayland" in clipboard, "clipboard did not reach the viewer")

    image = io.BytesIO()
    Image.new("RGB", (12, 8), (71, 82, 93)).save(image, format="PNG")
    png = image.getvalue()
    await ws.send(bytes([0x11]) + png)
    await asyncio.sleep(0.15)
    result = subprocess.check_output(["wl-paste", "--type", "image/png"], env=env, timeout=5)
    assert result == png, "image clipboard differs"
    image = io.BytesIO()
    Image.new("RGB", (13, 9), (93, 82, 71)).save(image, format="PNG")
    native_png = image.getvalue()
    subprocess.run(["wl-copy", "--type", "image/png"], input=native_png, env=env, check=True, timeout=5)
    await until(lambda: native_png in clipboard, "native image clipboard did not reach the viewer")

    await asyncio.sleep(0.6)
    before = json.loads(get("/stats"))["encoded"]
    await asyncio.sleep(0.6)
    after = json.loads(get("/stats"))["encoded"]
    assert after - before <= 1, ("idle screen keeps encoding", before, after)
    wait = json.loads(get("/wait?idle=100&timeout=1000"))
    assert not wait["timed_out"], wait

    ipc({"request":"output-config", "name":"SECOND", "mode":"1024x768"})
    await until(lambda: (1024, 768) in geometries and decoded[-1].width == 1024
                and decoded[-1].height == 768, "capture did not follow a display mode change")
    assert Image.open(io.BytesIO(get("/screenshot"))).size == (1024, 768)
    ipc({"request":"output-config", "name":"SECOND", "transform":"90"})
    await until(lambda: (768, 1024) in geometries and decoded[-1].width == 768
                and decoded[-1].height == 1024, "capture did not follow output rotation")

    # Read-only serving needs no input IPC and rejects raw input commands.
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        readonly_port = s.getsockname()[1]
    readonly = start("readonly", [os.environ.get("VOXL_BIN", str(ROOT / "target/debug/voxl")),
        "--capture-backend=wayland", "--wayland-output=SECOND", "--read-only", "--aizome-socket=/no/such/socket",
        f"--port={readonly_port}", "--audio-enabled=false", "--file-transfers=none"])
    await until(lambda: b"listening on" in (EVIDENCE / "readonly.log").read_bytes(), "read-only server required input access")
    rw = await websockets.connect(f"ws://127.0.0.1:{readonly_port}/ws?token=isolated-native-test")
    assert json.loads(await rw.recv())["readOnly"]
    for command in ["k,65507,1", "b,1,1", "c,forbidden", "sz,640,480"]:
        await rw.send(command)
    await asyncio.sleep(0.1)
    state = ipc({"request":"input-state"})["state"]
    assert not state["pressed_keys"] and not state["pressed_buttons"], state
    await rw.close()
    readonly.terminate()
    readonly.wait(timeout=3)

    # Disconnect releases keys/buttons even if the browser never sent their ups.
    await ws.send("k,65507,1")
    await ws.send("b,1,1")
    await asyncio.sleep(0.1)
    await ws.close()
    try:
        await reader
    except websockets.exceptions.ConnectionClosedError:
        pass  # Voxl closes its writer when it sees the client's close.
    await until(lambda: not ipc({"request":"input-state"})["state"]["pressed_keys"]
                and not ipc({"request":"input-state"})["state"]["pressed_buttons"], "input remained held after disconnect")

    # Aizome's Stop Sharing is authoritative; the backend must not recreate it.
    await asyncio.sleep(1.1)
    revoking = await websockets.connect(f"ws://127.0.0.1:{port}/ws?token=isolated-native-test")
    await revoking.recv()
    await revoking.send("k,65507,1")
    await revoking.send("b,1,1")
    await until(lambda: ipc({"request":"input-state"})["state"]["pressed_keys"]
                and ipc({"request":"input-state"})["state"]["pressed_buttons"], "revocation precondition: input not held")
    ipc({"request":"stop-share"})
    await until(lambda: server.poll() is not None, "server restarted a revoked share")
    state = ipc({"request":"input-state"})["state"]
    assert not state["pressed_keys"] and not state["pressed_buttons"], state
    await revoking.close()
    report = {"capture":"native Wayland ext-image-copy-capture", "xwayland":False,
              "aizome":version,
              "renderer":os.environ.get("AIZOME_TEST_RENDERER", "pixman"), "output":output,
              "checks":["H.264 decode", "selected scaled output", "native pixels", "keyboard ASCII/shift/Unicode",
                        "pointer/buttons/scroll", "clipboard text both ways", "clipboard image both ways",
                        "idle capture", "/wait", "display resize and rotation", "read-only input refusal",
                        "disconnect release", "share revocation"]}
    if os.environ.get("VOXL_BROWSER"):
        report["checks"].append("Chrome WebCodecs pixels and keyboard/IME input")
    (EVIDENCE / "result.json").write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report, indent=2))


try:
    asyncio.run(main())
finally:
    for child in reversed(children):
        if child.poll() is None:
            child.terminate()
            try: child.wait(timeout=3)
            except subprocess.TimeoutExpired: child.kill(); child.wait()
    print("Evidence:", EVIDENCE)
