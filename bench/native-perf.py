"""Reproducible content-to-compositor measurements, on two private Aizomes.

The exact same independent Wayland marker observer measures the local control,
browser and native viewer. No window of the login desktop is read or modified.
Results are component/headless evidence, not physical display latency.
"""
import argparse
import asyncio
import hashlib
import json
import math
import os
from pathlib import Path
import socket
import shutil
import subprocess
import tempfile
import time
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
parser = argparse.ArgumentParser()
parser.add_argument("--viewer", choices=["local", "browser", "cpu", "cuda", "auto"], default="cpu")
parser.add_argument("--source", choices=["gl", "sukumo"], default="gl")
parser.add_argument("--native-widgets", action="store_true")
parser.add_argument("--scene-compression",choices=["none","zlib"],default="zlib")
parser.add_argument("--glyph-transfer",choices=["mask","font"],default="mask",help="Sukumo cached coverage or full-font baseline")
parser.add_argument("--link-mbps",type=float,default=0,help="private reliable-link byte cap per direction; zero means uncapped")
parser.add_argument("--link-delay-ms",type=float,default=0,help="private link propagation delay per direction")
parser.add_argument("--encoder", choices=["cpu", "gpu"], default="cpu")
parser.add_argument("--size", default="1920x1080")
parser.add_argument("--workload", choices=["scroll", "animation", "drag", "idle"], default="scroll")
parser.add_argument("--fps", type=int, default=60)
parser.add_argument("--marker-cell",type=int,default=8,choices=range(8,33))
parser.add_argument("--seconds", type=int, default=15)
parser.add_argument("--warmup", type=int, default=5)
parser.add_argument("--kbps", type=int, default=30000)
parser.add_argument("--stripes", type=int, default=1)
parser.add_argument("--crf", type=float, default=24)
parser.add_argument("--gpu-preset", default="P1")
parser.add_argument("--quality", action="store_true",help="separate image-quality run; does not collect timing/CPU scores")
args = parser.parse_args()
if not all(math.isfinite(v) and v >= 0 for v in (args.link_mbps,args.link_delay_ms)):
    parser.error("link rate and delay must be finite and nonnegative")
if args.native_widgets and (args.source!="sukumo" or args.viewer in ("local","browser")):
    parser.error("native widgets require the Sukumo source and a native viewer")
if args.quality and (args.source!="gl" or args.viewer=="local" or args.stripes!=1):
    parser.error("quality runs require a streamed viewer and one stripe")
if (args.link_mbps or args.link_delay_ms) and (args.quality or args.viewer=="local"):
    parser.error("link emulation requires a streamed timing run")
width, height = map(int, args.size.split("x"))
if 16+56*args.marker_cell>width or (args.source=="sukumo" and args.marker_cell!=8):
    parser.error("marker must fit the output; Sukumo fixture uses 8-pixel cells")
EVIDENCE = Path(tempfile.mkdtemp(prefix="voxl-perf-"))
children = []
link = None
env = os.environ.copy()
for key in ("DISPLAY", "WAYLAND_SOCKET", "AIZOME_SOCKET", "WAYLAND_DISPLAY", "AIZOME_LOGIC"):
    env.pop(key, None)
for key in ("XDG_RUNTIME_DIR", "XDG_CONFIG_HOME", "XDG_STATE_HOME", "XDG_DATA_HOME", "XDG_CACHE_HOME", "HOME"):
    directory = EVIDENCE / key.lower()
    directory.mkdir(mode=0o700)
    env[key] = str(directory)
env.update(VOXL_PASSWORD="private-performance-fixture", RUST_LOG=os.environ.get("VOXL_BENCH_LOG","info"), SDL_VIDEODRIVER="wayland")


def environment(name):
    return dict(env, WAYLAND_DISPLAY=name, AIZOME_SOCKET=f"{env['XDG_RUNTIME_DIR']}/aizome.{name}.sock")


def start(name, command, environment):
    package_libs=os.environ.get("VOXL_PACKAGE_LIBRARY_PATH")
    if package_libs is not None and command[0] in (os.environ.get("VOXL_BIN"),os.environ.get("VOXL_VIEWER")):
        environment=dict(environment,LD_LIBRARY_PATH=package_libs)
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
    global link
    binaries={}
    for name,path in {
        "server":os.environ.get("VOXL_BIN",str(ROOT/"target/release/voxl")),
        "viewer":os.environ.get("VOXL_VIEWER",str(ROOT/"native/target/release/voxl-viewer")),
        "scene":os.environ["AIZOME_SCENE_FIXTURE"] if args.source=="sukumo" else os.environ.get("VOXL_SCENE",str(ROOT/"native/target/release/voxl-scene")),
        "observer":str(ROOT/"target/release/examples/wayland-observe"),
    }.items():
        binaries[name]={"path":path,"sha256":hashlib.sha256(Path(path).read_bytes()).hexdigest()}
    if args.native_widgets:
        renderer=os.environ["AIZOME_REMOTE_RENDERER"]
        binaries["retained_renderer"]={"path":renderer,"sha256":hashlib.sha256(Path(renderer).read_bytes()).hexdigest()}
    compositor = Path(shutil.which(os.environ.get("AIZOME_BIN", "aizome"))).resolve()
    for path in [compositor, compositor.parent/"libaizome_comp_core.so", compositor.parent/"libaizome_comp_logic.so"]:
        if path.is_file():
            binaries["compositor_"+path.name]={"path":str(path),"sha256":hashlib.sha256(path.read_bytes()).hexdigest()}
    source, receiver = environment("source"), environment("receiver")
    roots = {}
    for name, environment_ in [("source", source), ("receiver", receiver)]:
        roots[name] = start(name, [os.environ.get("AIZOME_BIN", "aizome"), "--backend", "headless",
            "--renderer", "gles", "--socket", name, "--no-session", "--no-shell", "--no-xwayland",
            "--headless-output", f"TEST={args.size}@1"], environment_).pid
        await until(lambda: ipc(environment_, {"request":"version"}), f"{name} unavailable")
    if args.source=="sukumo":
        roots["scene"]=start("scene",[os.environ["AIZOME_SCENE_FIXTURE"]],dict(source,AIZOME_SCENE_BENCH="1",AIZOME_REMOTE_GLYPHS=args.glyph_transfer)).pid
    else:
        roots["scene"] = start("scene", [os.environ.get("VOXL_SCENE",str(ROOT/"native/target/release/voxl-scene")),
            str(width),str(height),str(args.fps),str(args.warmup+args.seconds+60),args.workload]
            + [f"--marker-cell={args.marker_cell}"]
            + ([f"--freeze-after={args.fps*(args.warmup+1)}"] if args.quality else []), source).pid
    await until(lambda: ipc(source,{"request":"windows"})["windows"], "scene unavailable")
    base = None
    viewer_started = None
    retained_startup_ms = None
    retained_startup_stats = None
    if args.viewer != "local":
        with socket.socket() as sock:
            sock.bind(("127.0.0.1",0));port=sock.getsockname()[1]
        base=f"http://127.0.0.1:{port}"
        roots["server"] = start("server", [os.environ.get("VOXL_BIN",str(ROOT/"target/release/voxl")),
            "--capture-backend=wayland",f"--video-backend={args.encoder}",f"--port={port}",
            "--audio-enabled=false","--file-transfers=none",f"--framerate={args.fps}",
            f"--max-kbps={args.kbps}",f"--crf={args.crf}",f"--gpu-quality={args.crf}",
            f"--stripes={args.stripes}",f"--gpu-preset={args.gpu_preset}",f"--native-widgets={str(args.native_widgets).lower()}"],source).pid
        await until(lambda:b"listening on" in (EVIDENCE/"server.log").read_bytes(),"server unavailable")
        address=base+"/?token=private-performance-fixture"
        if args.link_mbps or args.link_delay_ms:
            from link import Link
            link=Link(port,args.link_mbps,args.link_delay_ms)
            proxy_port=await link.start()
            address=f"http://127.0.0.1:{proxy_port}/?token=private-performance-fixture"
        viewer_started = time.monotonic()
        if args.viewer=="browser":
            profile=EVIDENCE/"chrome-profile"
            roots["viewer"]=start("viewer",[os.environ.get("VOXL_BROWSER","google-chrome"),
                "--ozone-platform=wayland","--kiosk",f"--app={address}",f"--window-size={width},{height}",
                f"--user-data-dir={profile}","--no-first-run","--no-default-browser-check",
                "--remote-debugging-port=0","--autoplay-policy=no-user-gesture-required"],receiver).pid
        else:
            roots["viewer"]=start("viewer",[os.environ.get("VOXL_VIEWER",str(ROOT/"native/target/release/voxl-viewer")),
                address,f"--decoder={args.viewer}","--fullscreen",f"--seconds={args.seconds+args.warmup+40}"] + (["--native-widgets",f"--scene-compression={args.scene_compression}"] if args.native_widgets else []),receiver).pid
        window=await until(lambda:next(iter(ipc(receiver,{"request":"windows"})["windows"]),None),"viewer unavailable")
        ipc(receiver,{"request":"fullscreen","id":window["id"],"state":True})
    def stats():
        if base is None:return {}
        req=urllib.request.Request(base+"/stats",headers={"Authorization":"Bearer private-performance-fixture"})
        with urllib.request.urlopen(req,timeout=5) as response:return json.load(response)
    if args.quality:
        from quality import measure
        report={"configuration":vars(args),"quality":await measure(base,EVIDENCE,(args.fps*(args.warmup+1)-1)&0xffff,args.marker_cell),
            "binaries":binaries,"aizome":ipc(source,{"request":"version"}),"evidence":str(EVIDENCE)}
        (EVIDENCE/"result.json").write_text(json.dumps(report,indent=2)+"\n")
        print(json.dumps(report,indent=2),flush=True)
        return
    if args.native_widgets:
        await until(lambda:"Aizome native widgets active" in (EVIDENCE/"viewer.log").read_text(),"retained replay not active")
        # Includes process/window/network/renderer startup, polled at 50 ms.
        # It is a coarse end-to-end readiness observation, not first scanout.
        retained_startup_ms = round((time.monotonic()-viewer_started)*1000)
        retained_startup_stats = stats()
    await asyncio.sleep(args.warmup)
    before_stats=stats()
    before={name:resources(pid) for name,pid in roots.items()}
    start_time=time.monotonic()
    observer=start("observer",[str(ROOT/"target/release/examples/wayland-observe"),str(args.seconds),str(args.marker_cell)],
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
               "rss_bytes":after[name]["rss_bytes"],"rss_start_bytes":before[name]["rss_bytes"],
               "rss_growth_bytes":after[name]["rss_bytes"]-before[name]["rss_bytes"]} for name in roots}
    delta={key:after_stats[key]-value for key,value in before_stats.items()
           if isinstance(value,(int,float)) and isinstance(after_stats.get(key),(int,float))}
    if args.native_widgets:
        assert delta.get("retained_frames",0)>0 and delta.get("bytes",0)==0, ("retained measurement fell back to video",delta)
    browser_stats=None
    browser_system=None
    # Preserve the viewer image before opening any diagnostic browser pages.
    subprocess.run(["grim","-o","TEST",str(EVIDENCE/"presented.png")],
                   env=source if args.viewer=="local" else receiver,check=True,timeout=5,capture_output=True)
    if args.viewer=="browser":
        from playwright.async_api import async_playwright
        port=int((profile/"DevToolsActivePort").read_text().splitlines()[0])
        async with async_playwright() as playwright:
            browser=await playwright.chromium.connect_over_cdp(f"http://127.0.0.1:{port}")
            cdp=await browser.new_browser_cdp_session()
            browser_system={"version":await cdp.send("Browser.getVersion"),"system":await cdp.send("SystemInfo.getInfo")}
            for page in browser.contexts[0].pages:
                if page.url.startswith(address.split("?",1)[0]):
                    browser_stats=await page.evaluate("({stats:window.voxlStats,width:innerWidth,height:innerHeight,dpr:devicePixelRatio})")
            media=await browser.contexts[0].new_page()
            await media.goto("chrome://media-internals")
            await media.wait_for_timeout(500)
            players=media.locator("#player-list .tree-item-header")
            for index in range(await players.count()):
                await players.nth(index).click()
                (EVIDENCE/f"browser-media-player-{index}.txt").write_text(await media.locator("body").inner_text())
            (EVIDENCE/"browser-media.html").write_text(await media.content())
            (EVIDENCE/"browser-media.txt").write_text(await media.locator("body").inner_text())
            await browser.close()
    reliable=observation["invalid"]==0
    quantiles={"p50":quantile(.5),"p95":quantile(.95),"p99":quantile(.99)}
    report={"configuration":vars(args),"measurement_valid":reliable,
        "package_library_path":os.environ.get("VOXL_PACKAGE_LIBRARY_PATH"),
        "retained_startup_ms":retained_startup_ms,
        "retained_startup_stats":retained_startup_stats,
        "visible_fps":len(age)/observation["seconds"] if reliable and args.workload!="idle" else None,
        "valid_marker_fps":len(age)/observation["seconds"],
        "age_ms":quantiles if reliable and args.workload!="idle" else {k:None for k in quantiles},
        "observation":{"captures":observation["captures"],"invalid":observation["invalid"],
                       "marker_valid_fraction":1-observation["invalid"]/max(1,observation["captures"])},
        "cpu":cpu,"server_delta":delta,"server_end":after_stats,"browser":browser_stats,"browser_system":browser_system,
        "binaries":binaries,"aizome":ipc(source,{"request":"version"}),"evidence":str(EVIDENCE)}
    if not reliable and args.workload!="idle":
        report["rejected_age_ms"]=quantiles
        report["measurement_rejection"]="Corrupted or expired markers can bias frame counts and admit false timestamps; latency/fps scores are invalid. Raw observations are retained for diagnosis."
    if args.workload=="idle":
        report["measurement_note"]="Static source: the old timestamp is not a latency sample. Inspect idle CPU and encoding/byte counters; no scene-age or frame-rate score is reported."
    if link is not None:
        report["link"]={"method":"Bounded private TCP proxy; fixed propagation and serialized byte rate per direction, no loss or TCP congestion emulation", "totals_including_warmup":link.stats}
    (EVIDENCE/"result.json").write_text(json.dumps(report,indent=2)+"\n")
    print(json.dumps(report,indent=2),flush=True)


async def run():
    try:
        await main()
    finally:
        if link is not None:await link.close()


if __name__=="__main__":
    try:
        asyncio.run(run())
    finally:
        for process in reversed(children):
            if process.poll() is None:
                process.terminate()
                try:process.wait(timeout=5)
                except subprocess.TimeoutExpired:process.kill();process.wait()
        print("Evidence:",EVIDENCE,flush=True)
