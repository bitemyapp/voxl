"""Real retained Sukumo painting, native viewer, and pixel fallback on private Aizomes.

Requires AIZOME_BIN, AIZOME_SCENE_FIXTURE and AIZOME_REMOTE_RENDERER from the
matching Sekka checkout. Does not touch the login compositor or host windows.
"""
import asyncio
import importlib.util
import json
import os
import re
from pathlib import Path
import socket
import subprocess
import time
import urllib.request
from PIL import Image, ImageChops, ImageStat
from websockets.asyncio.client import connect
spec=importlib.util.spec_from_file_location("native_smoke",Path(__file__).with_name("native-smoke.py"))
f=importlib.util.module_from_spec(spec);spec.loader.exec_module(f)

def shot(env,name):
    path=f.EVIDENCE/(name+".png")
    result=subprocess.run(["grim","-o","TEST",str(path)],env=env,capture_output=True,timeout=5)
    assert result.returncode==0,result.stderr.decode()
    return Image.open(path).convert("RGB")

def key(env,code):
    return f.ipc(env,{"request":"synthesize","events":[{"type":"key","key":str(code),"pressed":True},{"type":"key","key":str(code),"pressed":False}]})

async def main():
    source,receiver=f.environment("source"),f.environment("receiver")
    scale=float(os.environ.get("VOXL_SCENE_SCALE","1"))
    for name,env in [("source",source),("receiver",receiver)]:
        f.start(name,[os.environ["AIZOME_BIN"],"--backend","headless","--renderer","gles","--socket",name,
            "--no-session","--no-shell","--no-xwayland","--headless-output",f"TEST=1280x800@{scale if name=='source' else 1}"],env)
        await f.until(lambda:f.ipc(env,{"request":"version"}),"private compositor unavailable")
    app=f.start("scene",[os.environ["AIZOME_SCENE_FIXTURE"]],source)
    window=await f.until(lambda:next((w for w in f.ipc(source,{"request":"windows"})["windows"] if w["app_id"]=="org.aizome.RemoteSceneFixture"),None),"Sukumo scene absent")
    f.ipc(source,{"request":"focus","id":window["id"]})
    with socket.socket() as sock:sock.bind(("127.0.0.1",0));port=sock.getsockname()[1]
    server=f.start("server",[os.environ.get("VOXL_BIN",str(f.ROOT/"target/release/voxl")),"--capture-backend=wayland",
        "--video-backend="+os.environ.get("VOXL_VIDEO","gpu"),f"--port={port}","--audio-enabled=false","--file-transfers=none","--show-cursor=false","--native-widgets=true"],source)
    await f.until(lambda:b"listening on" in (f.EVIDENCE/"server.log").read_bytes(),"Voxl absent")
    viewer=f.start("viewer",[os.environ.get("VOXL_VIEWER",str(f.ROOT/"native/target/release/voxl-viewer")),
        f"http://127.0.0.1:{port}/?token=isolated-native-viewer","--fullscreen","--native-widgets","--no-audio","--seconds=55"],receiver)
    log=lambda:(f.EVIDENCE/"viewer.log").read_text()
    await f.until(lambda:"Aizome native widgets active" in log(),"native scene did not become active")
    vwindow=await f.until(lambda:next((w for w in f.ipc(receiver,{"request":"windows"})["windows"] if "Voxl" in w.get("title","")),None),"viewer window absent")
    f.ipc(receiver,{"request":"focus","id":vwindow["id"]})
    checks=[]
    def exact(name):
        a,b=shot(source,name+"-source"),shot(receiver,name+"-viewer")
        diff=ImageChops.difference(a,b)
        mean=ImageStat.Stat(diff).mean
        diff.save(f.EVIDENCE/(name+"-difference.png"))
        return {"mean_rgb":mean,"maximum_rgb":diff.getextrema(),"exact":diff.getbbox() is None}
    await asyncio.sleep(.2)
    first=exact("full")
    assert first["exact"],first
    checks.append("pixel-exact full Sukumo frame including real font glyphs")
    # A second, ordinary read-only share must stay pixel-only even when an
    # authenticated peer requests richer drawing assets. The source owner did
    # not grant --native-widgets on this server.
    with socket.socket() as sock:sock.bind(("127.0.0.1",0));pixel_port=sock.getsockname()[1]
    pixel_server=f.start("pixel-only-server",[os.environ.get("VOXL_BIN",str(f.ROOT/"target/release/voxl")),
        "--capture-backend=wayland","--video-backend=cpu",f"--port={pixel_port}","--audio-enabled=false",
        "--file-transfers=none","--read-only=true","--show-cursor=false"],source)
    try:
        await f.until(lambda:b"listening on" in (f.EVIDENCE/"pixel-only-server.log").read_bytes(),"pixel-only server absent")
        async with connect(f"ws://127.0.0.1:{pixel_port}/ws?token=isolated-native-viewer",max_size=32*1024*1024) as ws:
            await ws.send("scene,1,17,zlib");await ws.send("v,1")
            denied=False;video=False
            async with asyncio.timeout(8):
                while not (denied and video):
                    packet=await ws.recv()
                    if isinstance(packet,bytes):
                        assert packet[0] not in (0x20,0x21),"pixel-only share leaked drawing assets"
                        if packet[0]==1:
                            video=True;await ws.send("a,"+str(int.from_bytes(packet[2:6],"big")))
                    else:
                        message=json.loads(packet)
                        denied |= message.get("t")=="scene" and message.get("nonce")==17 and message.get("reason")=="server-native-widgets-disabled"
        assert "retained capture object=" not in (f.EVIDENCE/"pixel-only-server.log").read_text()
    finally:
        pixel_server.terminate();pixel_server.wait(timeout=5)
    checks.append("source-side default denies drawing assets while read-only video remains available")
    # The incoming input still reaches the original app; only its damaged button
    # is replayed, with all untouched text and paths remaining exactly unchanged.
    f.ipc(receiver,{"request":"synthesize","events":[{"type":"pointer-motion","x":180,"y":200},
        {"type":"pointer-button","button":"272","pressed":True},{"type":"pointer-button","button":"272","pressed":False}]})
    await f.until(lambda:"button green=true" in (f.EVIDENCE/"scene.log").read_text(),"remote button input absent")
    await f.until(lambda:shot(receiver,"button-wait").getpixel((100,180))[1]>150,"native damage absent")
    damage=exact("damage");assert damage["exact"],damage
    checks.append("pixel-exact partial damage and input to original app")
    def stats():
        with urllib.request.urlopen(f"http://127.0.0.1:{port}/stats?token=isolated-native-viewer",timeout=3) as r:return json.load(r)
    before=stats();key(receiver,30);await asyncio.sleep(.5);a=stats();await asyncio.sleep(.5);b=stats();key(receiver,30)
    assert b["encoded"]==a["encoded"],(a,b)
    checks.append("active retained animation stops video encoding for this sole viewer")
    # A simultaneous browser-compatible pixel subscriber must continue to get
    # normal video while the native viewer independently uses retained paint.
    async with connect(f"ws://127.0.0.1:{port}/ws?token=isolated-native-viewer",max_size=32*1024*1024) as ws:
        await ws.send("v,1")
        received=[]
        async def consume():
            async for packet in ws:
                if isinstance(packet,bytes) and packet[0]==1:
                    received.append(packet)
                    await ws.send("a,"+str(int.from_bytes(packet[2:6],"big")))
        reader=asyncio.create_task(consume())
        key(receiver,30);await asyncio.sleep(.7);key(receiver,30)
        assert len(received)>15,len(received)
        reader.cancel()
    await asyncio.sleep(.2)
    assert exact("mixed-client")["exact"]
    checks.append("mixed retained and ordinary video subscribers remain independent")
    # F9 is a live, reversible user toggle. Ordinary H.264 continues working.
    key(receiver,67);await asyncio.sleep(.3)
    key(receiver,30);await asyncio.sleep(.4);c=stats();key(receiver,30)
    assert c["encoded"]>b["encoded"],(b,c)
    checks.append("F9 restores ordinary video and encoding")
    count=log().count("Aizome native widgets active");key(receiver,67)
    await f.until(lambda:log().count("Aizome native widgets active")>count,"F9 did not restore retained replay")
    await asyncio.sleep(.2);assert exact("retoggled")["exact"]
    checks.append("F9 requests a complete fresh retained dictionary")
    # A real popup is a different surface and must force the whole view to video.
    previous=log().count("scene-requires-pixels");key(receiver,25)
    await f.until(lambda:log().count("scene-requires-pixels")>previous,"popup did not force pixel fallback")
    await f.until(lambda:shot(receiver,"popup").getpixel((int(700*scale),int(200*scale)))[0]>170,"popup missing in pixel fallback")
    checks.append("unsupported popup falls back to video without losing its pixels")
    # Closing it requests a fresh, complete native frame again.
    count=log().count("Aizome native widgets active")
    key(source,25)
    await f.until(lambda:log().count("Aizome native widgets active")>count,"closing popup did not restore native scene")
    await asyncio.sleep(.2);assert exact("popup-closed")["exact"]
    checks.append("scene recovery after popup closure")
    # Rapid changes coalesce into one IPC worker. A client that does not ack
    # cannot build an unbounded retained queue or stall the other native viewer.
    threads_before=len(list(Path(f"/proc/{server.pid}/task").iterdir()))
    async with connect(f"ws://127.0.0.1:{port}/ws?token=isolated-native-viewer",max_size=32*1024*1024) as ws:
        for nonce in range(1,201):await ws.send(f"scene,1,{nonce}")
        key(receiver,30)
        await f.until(lambda:"fell behind" in (f.EVIDENCE/"server.log").read_text() or "acknowledgement timed out" in (f.EVIDENCE/"server.log").read_text(),"stalled scene did not fall back",8)
        threads_after=len(list(Path(f"/proc/{server.pid}/task").iterdir()))
        assert threads_after<=threads_before+2,(threads_before,threads_after)
        key(receiver,30)
    await asyncio.sleep(.3)
    assert exact("after-stalled-client")["exact"]
    checks.append("rapid toggles use one worker and stalled replay falls back with bounded queues")
    # A peer can acknowledge steadily while still consuming slower than the
    # producer. This must not turn into an ever older retained animation.
    async with connect(f"ws://127.0.0.1:{port}/ws?token=isolated-native-viewer",max_size=32*1024*1024) as ws:
        await ws.send("scene,1,301,zlib")
        async def slow_ack():
            async for packet in ws:
                if isinstance(packet,bytes) and packet[0] in (0x20,0x21):
                    await asyncio.sleep(.15)
                    await ws.send(f"sa,{int.from_bytes(packet[1:5],'little')},{int.from_bytes(packet[5:9],'little')}")
        reader=asyncio.create_task(slow_ack())
        key(receiver,30)
        try:
            await f.until(lambda:"retained producer outran link" in (f.EVIDENCE/"server.log").read_text(),"slow acknowledging peer accumulated stale replay",8)
        finally:
            key(receiver,30)
            reader.cancel()
            await asyncio.gather(reader,return_exceptions=True)
    await asyncio.sleep(.3)
    assert exact("after-slow-client")["exact"]
    checks.append("sustained backpressure resumes video even when the slow peer keeps acknowledging")
    # Another Unix process cannot claim the server's capture object.
    denied=None
    with socket.socket(socket.AF_UNIX) as s:
        s.connect(source["AIZOME_SOCKET"]);r=s.makefile("rb")
        s.sendall(b'{"request":"subscribe"}\n');r.readline()
        object_id=int(re.search(r"retained capture object=(\d+)",(f.EVIDENCE/"server.log").read_text())[1])
        s.sendall(json.dumps({"request":"remote-scene","capture":object_id,"output":"TEST","enabled":True}).encode()+b"\n")
        denied=json.loads(r.readline())
    assert denied["response"]=="error",denied
    checks.append("retained capture subscription rejects unrelated peer")
    f.ipc(source,{"request":"stop-share"})
    await f.until(lambda:"capture-stopped" in log() or "Disconnected:" in log() or viewer.poll() is not None,"revocation did not stop scene")
    checks.append("share revocation ends retained stream")
    result={"checks":checks,"scale":scale,"full":first,"damage":damage,"evidence":str(f.EVIDENCE)}
    (f.EVIDENCE/"scene-result.json").write_text(json.dumps(result,indent=2)+"\n")
    print(json.dumps(result,indent=2))

if __name__=="__main__":
    try:asyncio.run(main())
    finally:
        for p in reversed(f.children):
            if p.poll() is None:
                p.terminate()
                try:p.wait(timeout=3)
                except subprocess.TimeoutExpired:p.kill();p.wait()
        print("Evidence:",f.EVIDENCE)
