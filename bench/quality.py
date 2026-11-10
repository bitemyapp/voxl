"""Compare encoded moving/idle-refined frames with an exact lossless source.

Used separately from timing runs: its extra decoder and readback deliberately
invalidate CPU/latency measurements. The private scene freezes at a fixed frame
so every encoder sees exactly the same content at the comparison point.
"""
import asyncio
from collections import deque
import io
import math
import re
import subprocess
import urllib.request

import av
import numpy as np
from PIL import Image
import websockets


def marker(plane, stride):
    word=0
    for bit in range(56):
        if plane[24*stride+(16+bit*8+4)]>125:word|=1<<bit
    raw=word.to_bytes(7,"little")
    check=0xa5
    for value in raw[:6]:check^=value
    return (word>>32)&0xffff if check==raw[6] else None


def score(reference,decoded,path):
    decoded.save(path)
    expected=np.asarray(reference,dtype=np.float32)
    actual=np.asarray(decoded,dtype=np.float32)
    error=np.abs(expected-actual)
    mse=float(np.mean(error*error))
    luma=expected[:,:,0]*.299+expected[:,:,1]*.587+expected[:,:,2]*.114
    edges=np.zeros(luma.shape,dtype=bool)
    edges[1:,:]|=np.abs(np.diff(luma,axis=0))>32
    edges[:,1:]|=np.abs(np.diff(luma,axis=1))>32
    ffmpeg=subprocess.run(["ffmpeg","-hide_banner","-nostdin","-i",str(path.parent/"reference.png"),
        "-i",str(path),"-lavfi","ssim","-f","null","-"],capture_output=True,text=True,timeout=30)
    (path.with_suffix(".ssim.log")).write_text(ffmpeg.stderr)
    assert ffmpeg.returncode==0,ffmpeg.stderr
    match=re.search(r"All:([0-9.]+)",ffmpeg.stderr)
    return {"rgb_psnr_db":10*math.log10(255*255/mse) if mse else None,
        "rgb_mean_absolute_error":float(np.mean(error)),"rgb_p95_absolute_error":float(np.percentile(error,95)),
        "edge_mean_absolute_error":float(np.mean(error[edges])) if np.any(edges) else 0,
        "ssim":float(match[1]) if match else None}


async def measure(base,evidence,target_frame):
    password="private-performance-fixture"
    frames=deque(maxlen=16)
    codec=av.CodecContext.create("h264","r")
    async with websockets.connect(base.replace("http://","ws://")+"/ws?token="+password,max_size=64<<20) as ws:
        await ws.recv() # Initial hello; the harness fixes geometry/one stripe.
        await ws.send("v,1")
        bitstream=(evidence/"quality.h264").open("wb")
        async def receive():
            async for message in ws:
                if isinstance(message,str):
                    import json
                    m=json.loads(message)
                    if m.get("t")=="ping":await ws.send("po,"+str(m["s"]))
                    continue
                if message[0]!=1:continue
                bitstream.write(message[16:])
                for frame in codec.decode(av.Packet(message[16:])):
                    plane=frame.planes[0]
                    fid=marker(memoryview(plane),plane.line_size)
                    if fid is not None:frames.append((fid,message[1],frame))
                await ws.send("a,"+str(int.from_bytes(message[2:6],"big")))
        reader=asyncio.create_task(receive())
        try:
            deadline=asyncio.get_running_loop().time()+30
            while not any(n==target_frame for n,_,_ in frames):
                if reader.done():await reader
                assert asyncio.get_running_loop().time()<deadline,"scene did not reach its fixed comparison frame"
                await asyncio.sleep(.01)
            # Let the last surface commit land before taking the reference.
            await asyncio.sleep(.04)
            request=urllib.request.Request(base+"/screenshot",headers={"Authorization":"Bearer "+password})
            with urllib.request.urlopen(request,timeout=5) as response:reference=Image.open(io.BytesIO(response.read())).convert("RGB")
            reference.save(evidence/"reference.png")
            # The marker's white/black luminance is independent of chroma format.
            gray=np.asarray(reference.convert("L")).reshape(-1)
            fid=marker(gray,reference.width)
            assert fid==target_frame,("reference differs from the fixed comparison frame",fid,target_frame)
            deadline=asyncio.get_running_loop().time()+2
            moving=None
            while asyncio.get_running_loop().time()<deadline:
                moving=next((frame for n,flags,frame in frames if n==fid and not flags&2),None)
                if moving is not None:break
                if reader.done():await reader
                await asyncio.sleep(.01)
            assert moving is not None,"last moving frame did not match the source reference"
            moving=moving.to_image()
            await asyncio.sleep(.6)
            refined=next((frame for n,_,frame in reversed(frames) if n==fid),None)
            assert refined is not None,"idle frame missing"
            refined=refined.to_image()
            return {"source_frame":fid,"moving":score(reference,moving,evidence/"moving.png"),
                "refined":score(reference,refined,evidence/"refined.png"),
                "method":"Exact fixed-frame RGB snapshot versus the last moving frame and idle-refined frame; separate from performance runs"}
        finally:
            reader.cancel()
            try:await reader
            except asyncio.CancelledError:pass
            bitstream.close()
