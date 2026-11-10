"""Reproducible content-to-compositor measurements, on two private Aizomes.

The exact same independent Wayland marker observer measures the local control,
browser and native viewer. No window of the login desktop is read or modified.
Results are component/headless evidence, not physical display latency.
"""
import argparse
import asyncio
import hashlib
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import time
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
parser = argparse.ArgumentParser()
parser.add_argument("--viewer", choices=["local", "browser", "cpu", "cuda", "auto"], default="cpu")
parser.add_argument("--encoder", choices=["cpu", "gpu"], default="cpu")
parser.add_argument("--size", default="1920x1080")
parser.add_argument("--workload", choices=["scroll", "animation", "drag", "idle"], default="scroll")
parser.add_argument("--fps", type=int, default=60)
parser.add_argument("--seconds", type=int, default=15)
parser.add_argument("--warmup", type=int, default=5)
parser.add_argument("--kbps", type=int, default=30000)
parser.add_argument("--stripes", type=int, default=1)
parser.add_argument("--crf", type=float, default=24)
parser.add_argument("--gpu-preset", default="P1")
parser.add_argument("--quality", action="store_true",help="separate image-quality run; does not collect timing/CPU scores")
args = parser.parse_args()
if args.quality and (args.viewer=="local" or args.stripes!=1):
    parser.error("quality runs require a streamed viewer and one stripe")
width, height = map(int, args.size.split("x"))
EVIDENCE = Path(tempfile.mkdtemp(prefix="voxl-perf-"))
children = []
env = os.environ.copy()
for key in ("DISPLAY", "WAYLAND_SOCKET", "AIZOME_SOCKET", "WAYLAND_DISPLAY", "AIZOME_LOGIC"):
    env.pop(key, None)
for key in ("XDG_RUNTIME_DIR", "XDG_CONFIG_HOME", "XDG_STATE_HOME", "XDG_DATA_HOME", "XDG_CACHE_HOME", "HOME"):
    directory = EVIDENCE / key.lower()
    directory.mkdir(mode=0o700)
    env[key] = str(directory)
env.update(VOXL_PASSWORD="private-performance-fixture", RUST_LOG="info", SDL_VIDEODRIVER="wayland")


def environment(name):
    return dict(env, WAYLAND_DISPLAY=name, AIZOME_SOCKET=f"{env['XDG_RUNTIME_DIR']}/aizome.{name}.sock")


def start(name, command, environment):
    with (EVIDENCE / (name + ".log")).open("wb") as output:
        process = subprocess.Popen(command, env=environment, stdout=output, stderr=subprocess.STDOUT)
    children.append(process)
    return process


def ipc(environment, request):
    with socket.socket(socket.AF_UNIX) as connection:
        connection.settimeout(3)
        connection.connect(environment["AIZOME_SOCKET"])
        connection.sendall(json.dumps(request).encode() + b"\n")
        result = json.loads(connection.makefile("rb").readline())
        assert result["response"] != "error", result
        return result


async def until(test, reason, timeout=20):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        try:
            value = test()
            if value:
                return value
        except (OSError, ValueError):
            pass
        await asyncio.sleep(.05)
    raise AssertionError(reason)


def resources(root):
    processes = {}
    for path in Path("/proc").iterdir():
        if not path.name.isdigit():
            continue
        try:
            # comm may contain spaces/parentheses; the fields follow its final ).
            stat = (path / "stat").read_text().rsplit(")", 1)[1].split()
            processes[int(path.name)] = (int(stat[1]), int(stat[11]) + int(stat[12]), int(stat[21]))
        except (OSError, ValueError, IndexError):
            pass
    included = {root}
    while True:
        more = {pid for pid, (parent, _, _) in processes.items() if parent in included}
        if more <= included:
            break
        included |= more
    return {"ticks": sum(processes[p][1] for p in included if p in processes),
            "rss_bytes": sum(processes[p][2] for p in included if p in processes) * os.sysconf("SC_PAGE_SIZE"),
            "pids": sorted(included)}


async def main():
    binaries={}
    for name,path in {
        "server":os.environ.get("VOXL_BIN",str(ROOT/"target/release/voxl")),
        "viewer":os.environ.get("VOXL_VIEWER",str(ROOT/"native/target/release/voxl-viewer")),
        "scene":os.environ.get("VOXL_SCENE",str(ROOT/"native/target/release/voxl-scene")),
        "observer":str(ROOT/"target/release/examples/wayland-observe"),
    }.items():
        binaries[name]={"path":path,"sha256":hashlib.sha256(Path(path).read_bytes()).hexdigest()}
    source, receiver = environment("source"), environment("receiver")
    roots = {}
    for name, environment_ in [("source", source), ("receiver", receiver)]:
        roots[name] = start(name, [os.environ.get("AIZOME_BIN", "aizome"), "--backend", "headless",
            "--renderer", "gles", "--socket", name, "--no-session", "--no-shell", "--no-xwayland",
            "--headless-output", f"TEST={args.size}@1"], environment_).pid
        await until(lambda: ipc(environment_, {"request":"version"}), f"{name} unavailable")
    roots["scene"] = start("scene", [os.environ.get("VOXL_SCENE",str(ROOT/"native/target/release/voxl-scene")),
        str(width),str(height),str(args.fps),str(args.warmup+args.seconds+60),args.workload]
        + ([f"--freeze-after={args.fps*(args.warmup+1)}"] if args.quality else []), source).pid
    await until(lambda: ipc(source,{"request":"windows"})["windows"], "scene unavailable")
    base = None
    if args.viewer != "local":
        with socket.socket() as sock:
            sock.bind(("127.0.0.1",0));port=sock.getsockname()[1]
        base=f"http://127.0.0.1:{port}"
        roots["server"] = start("server", [os.environ.get("VOXL_BIN",str(ROOT/"target/release/voxl")),
            "--capture-backend=wayland",f"--video-backend={args.encoder}",f"--port={port}",
            "--audio-enabled=false","--file-transfers=none",f"--framerate={args.fps}",
            f"--max-kbps={args.kbps}",f"--crf={args.crf}",f"--gpu-quality={args.crf}",
            f"--stripes={args.stripes}",f"--gpu-preset={args.gpu_preset}"],source).pid
        await until(lambda:b"listening on" in (EVIDENCE/"server.log").read_bytes(),"server unavailable")
        address=base+"/?token=private-performance-fixture"
        if args.viewer=="browser":
            profile=EVIDENCE/"chrome-profile"
            roots["viewer"]=start("viewer",[os.environ.get("VOXL_BROWSER","google-chrome"),
                "--ozone-platform=wayland","--kiosk",f"--app={address}",f"--window-size={width},{height}",
                f"--user-data-dir={profile}","--no-first-run","--no-default-browser-check",
                "--remote-debugging-port=0","--autoplay-policy=no-user-gesture-required"],receiver).pid
        else:
            roots["viewer"]=start("viewer",[os.environ.get("VOXL_VIEWER",str(ROOT/"native/target/release/voxl-viewer")),
                address,f"--decoder={args.viewer}","--fullscreen",f"--seconds={args.seconds+args.warmup+40}"],receiver).pid
        window=await until(lambda:next(iter(ipc(receiver,{"request":"windows"})["windows"]),None),"viewer unavailable")
        ipc(receiver,{"request":"fullscreen","id":window["id"],"state":True})
    def stats():
        if base is None:return {}
        req=urllib.request.Request(base+"/stats",headers={"Authorization":"Bearer private-performance-fixture"})
        with urllib.request.urlopen(req,timeout=5) as response:return json.load(response)
    if args.quality:
        from quality import measure
        report={"configuration":vars(args),"quality":await measure(base,EVIDENCE,(args.fps*(args.warmup+1)-1)&0xffff),
            "binaries":binaries,"aizome":ipc(source,{"request":"version"}),"evidence":str(EVIDENCE)}
        (EVIDENCE/"result.json").write_text(json.dumps(report,indent=2)+"\n")
        print(json.dumps(report,indent=2),flush=True)
        return
    await asyncio.sleep(args.warmup)
    before_stats=stats()
    before={name:resources(pid) for name,pid in roots.items()}
    start_time=time.monotonic()
    observer=start("observer",[str(ROOT/"target/release/examples/wayland-observe"),str(args.seconds)],
                   source if args.viewer=="local" else receiver)
    await until(lambda:observer.poll() is not None,"observer stalled",args.seconds+15)
    assert observer.returncode==0,(EVIDENCE/"observer.log").read_text()
    elapsed=time.monotonic()-start_time
    after={name:resources(pid) for name,pid in roots.items()}
    after_stats=stats()
    observation=json.loads((EVIDENCE/"observer.log").read_text())
    age=sorted(sample["age_ms"] for sample in observation["samples"])
    assert len(age)>0 or args.workload=="idle","no valid displayed frame markers"
    quantile=lambda p:age[min(len(age)-1,int(len(age)*p))] if age else None
    cpu={name:{"core_percent":100*(after[name]["ticks"]-before[name]["ticks"])/os.sysconf("SC_CLK_TCK")/elapsed,
               "rss_bytes":after[name]["rss_bytes"]} for name in roots}
    delta={key:after_stats[key]-value for key,value in before_stats.items()
           if isinstance(value,(int,float)) and isinstance(after_stats.get(key),(int,float))}
    browser_stats=None
    if args.viewer=="browser":
        from playwright.async_api import async_playwright
        port=int((profile/"DevToolsActivePort").read_text().splitlines()[0])
        async with async_playwright() as playwright:
            browser=await playwright.chromium.connect_over_cdp(f"http://127.0.0.1:{port}")
            for page in browser.contexts[0].pages:
                if page.url.startswith(base):
                    browser_stats=await page.evaluate("({stats:window.voxlStats,width:innerWidth,height:innerHeight,dpr:devicePixelRatio})")
            await browser.close()
    subprocess.run(["grim","-o","TEST",str(EVIDENCE/"presented.png")],
                   env=source if args.viewer=="local" else receiver,check=True,timeout=5,capture_output=True)
    report={"configuration":vars(args),"visible_fps":len(age)/observation["seconds"],
        "age_ms":{"p50":quantile(.5),"p95":quantile(.95),"p99":quantile(.99)},
        "observation":{"captures":observation["captures"],"invalid":observation["invalid"]},
        "cpu":cpu,"server_delta":delta,"browser":browser_stats,
        "binaries":binaries,"aizome":ipc(source,{"request":"version"}),"evidence":str(EVIDENCE)}
    (EVIDENCE/"result.json").write_text(json.dumps(report,indent=2)+"\n")
    print(json.dumps(report,indent=2),flush=True)


if __name__=="__main__":
    try:
        asyncio.run(main())
    finally:
        for process in reversed(children):
            if process.poll() is None:
                process.terminate()
                try:process.wait(timeout=5)
                except subprocess.TimeoutExpired:process.kill();process.wait()
        print("Evidence:",EVIDENCE,flush=True)
