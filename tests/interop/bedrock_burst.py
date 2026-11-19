"""Drive a simultaneous Bedrock join and play benchmark in a disposable flat world."""
import argparse
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import threading
import time

from shared_resources import configure, stop_server

parser = argparse.ArgumentParser()
parser.add_argument("--server", type=Path, required=True)
parser.add_argument("--wine", type=Path)
parser.add_argument("--world", type=Path, required=True, help="stopped flat test world")
parser.add_argument("--client", type=Path, required=True, help="built cmd/burst executable")
parser.add_argument("--rpg", type=Path, required=True)
parser.add_argument("--players", type=int, default=100)
parser.add_argument("--play", type=int, default=60)
parser.add_argument("--pigs", type=int, default=64)
parser.add_argument("--port", type=int, default=19132)
parser.add_argument("--threshold", type=int, default=256)
parser.add_argument("--output", type=Path, required=True)
args = parser.parse_args()
if not 2 <= args.players <= 250 or args.play < 4 or not 0 <= args.pigs <= 256:
    parser.error("players 2–250, play at least four seconds, pigs 0–256")
out = args.output.resolve()
out.parent.mkdir(parents=True, exist_ok=True)
with tempfile.TemporaryDirectory(prefix="sculith-burst-") as tmp:
    root = Path(tmp)
    shutil.copytree(args.world.resolve(), root / "world")
    (root / "plugins").mkdir()
    shutil.copy2(args.rpg, root / "plugins/sculith-rpg.wasm")
    server_cmd = ([str(args.wine.resolve())] if args.wine else []) + [str(args.server.resolve())]
    env = dict(os.environ, RUST_LOG="info", NO_COLOR="1", RUST_BACKTRACE="1")
    subprocess.run(server_cmd + ["--init-settings"], cwd=root, env=env, check=True, stdout=subprocess.DEVNULL, timeout=90)
    cfg = root / "sculith.toml"
    if not cfg.exists():
        cfg = root / "pumpkin.toml"
    configure(cfg, {
        ("", "allow_nether"): "false", ("", "allow_end"): "false", ("", "default_gamemode"): '"Survival"',
        ("", "force_gamemode"): "true", ("world", "enable_spawn_chunks"): "false",
        ("logging", "color"): "false", ("networking.java", "enabled"): "false",
        ("networking.bedrock", "online_mode"): "false", ("networking.bedrock.authentication", "enabled"): "false",
        ("networking.bedrock.nethernet", "address"): f'"127.0.0.1:{args.port}"', ("networking.bedrock.nethernet","external_ip"): '"127.0.0.1"',
        ("networking.bedrock","view_distance"): "16", ("networking.bedrock","simulation_distance"): "4",
        ("networking.bedrock.compression","threshold"): str(args.threshold),
        ("networking.lan_broadcast","enabled"): "false", ("networking.rcon","enabled"): "false",
    })
    log_path = out.with_suffix(".server.log")
    samples = []
    started = threading.Event()
    built = threading.Event()
    loaded = threading.Event()
    phase = "setup"
    with log_path.open("w") as log:
        server = subprocess.Popen(server_cmd, cwd=root, env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                  stderr=subprocess.STDOUT, text=True, errors="replace", bufsize=1)
        def capture():
            for line in server.stdout:
                log.write(line)
                log.flush()
                if "Started server" in line:
                    started.set()
                if "The time is " in line: loaded.set()
                if "players online" in line:
                    built.set()
                m = re.search(r"SCULITH_TICK sample=(\d+) p50=([.\d]+) p95=([.\d]+) p99=([.\d]+) max=([.\d]+)", line)
                if m and phase in ("join","play"):
                    samples.append(dict(phase=phase,n=int(m[1]),p50_ms=float(m[2]),p95_ms=float(m[3]),p99_ms=float(m[4]),max_ms=float(m[5])))
        reader = threading.Thread(target=capture,daemon=True)
        reader.start()
        lock = threading.Lock()
        def command(line):
            with lock:
                server.stdin.write(line + "\n")
                server.stdin.flush()
        def wait(event, duration):
            end=time.monotonic()+duration
            while not event.wait(0.1):
                if server.poll() is not None or time.monotonic()>end:
                    raise RuntimeError(f"Server stopped or timed out. See {log_path}")
        fin = threading.Event()
        builder = None
        try:
            wait(started,120)
            builder = subprocess.Popen([str(args.client.resolve()),"-address",f"127.0.0.1:{args.port}","-prepare"], stdin=subprocess.PIPE, stdout=subprocess.PIPE,stderr=subprocess.STDOUT,text=True)
            with out.with_suffix(".setup.log").open("w") as setup_log:
                for line in builder.stdout:
                    setup_log.write(line)
                    setup_log.flush()
                    if line.startswith("ARENA_BUILDER_READY"): break
                else: raise RuntimeError(f"Arena preparation client failed. See {setup_log.name}")
            command("forceload add -16 -16 16 16")
            deadline=time.monotonic()+50
            while not loaded.is_set() and time.monotonic()<deadline:
                command("execute if loaded -16 -60 -16 if loaded 16 -60 16 run time query daytime")
                loaded.wait(0.3)
            if not loaded.is_set():
                raise RuntimeError(f"Test arena chunks did not load. See {log_path}")
            for line in [
                "gamerule minecraft:spawn_mobs false", "gamerule minecraft:respawn_radius 0",
                "gamerule minecraft:max_entity_cramming 0", "gamerule minecraft:advance_time false",
                "fill -16 -60 -16 16 -60 16 minecraft:stone",
                "fill -16 -59 -16 16 -55 16 minecraft:air", "setworldspawn 0 -59 0",
                "fill -16 -59 -16 16 -59 -16 minecraft:stone", "fill -16 -59 16 16 -59 16 minecraft:stone",
                "fill -16 -59 -15 -16 -59 15 minecraft:stone", "fill 16 -59 -15 16 -59 15 minecraft:stone",
            ]:
                command(line)
            for i in range(args.pigs):
                command(f"summon minecraft:pig {i%8-4} -59 {i//8-4}")
            command("list")
            wait(built,90)
            builder.stdin.close()
            builder.wait(timeout=15)
            time.sleep(1)
            def tick_samples():
                while not fin.wait(1):
                    command("sculith performance")
            meter=threading.Thread(target=tick_samples,daemon=True)
            phase="join"
            meter.start()
            client=subprocess.Popen([str(args.client.resolve()),"-address",f"127.0.0.1:{args.port}","-players",str(args.players),
                "-output",str(out),"-play",f"{args.play}s"],stdout=subprocess.PIPE,stderr=subprocess.STDOUT,text=True)
            try:
                with out.with_suffix(".clients.log").open("w") as client_log:
                    for line in client.stdout:
                        client_log.write(line)
                        client_log.flush()
                        if line.startswith("PLAY "): phase="play"
                        if line.startswith("MARK "):
                            wave=int(line.split()[1])
                            command(f"setblock {3+wave} -59 4 minecraft:gold_block")
                result=client.wait(timeout=10)
            finally:
                if client.poll() is None: client.kill(); client.wait()
                fin.set()
                meter.join()
            if not out.exists():
                raise RuntimeError(f"No client report. See {out.with_suffix('.clients.log')}")
            report=json.loads(out.read_text())
            report.update(tick_windows=samples, view_chunks=16,simulation_chunks=4,pigs_requested=args.pigs,
                compression_threshold=args.threshold,execution="Windows x64 under Wine" if args.wine else "native",
                motion_budget_ms=150,tick_budget_ms=50)
            out.write_text(json.dumps(report,indent=2)+"\n")
            print(json.dumps({k:v for k,v in report.items() if k not in ("tick_windows","client_errors")},ensure_ascii=False), flush=True)
            if result or (args.pigs and report["viewers_receiving_pigs"]!=args.players) or report["rpg_replies"]!=args.players or report["viewers_receiving_three_markers"]!=args.players:
                raise SystemExit(1)
        finally:
            fin.set()
            if builder is not None and builder.poll() is None:
                builder.kill()
                builder.wait()
            stop_server(server)
            reader.join(timeout=10)
