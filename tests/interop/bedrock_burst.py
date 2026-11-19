"""Drive a simultaneous Bedrock join and play benchmark on a disposable world copy."""
import argparse
import hashlib
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


CPU_QUOTA_COUNTERS = {
    "nr_periods": "host_cpu_quota_periods",
    "nr_throttled": "host_cpu_throttled_periods",
    "throttled_usec": "host_cpu_throttled_usec",
}


def host_counters(server_pid, client_pid):
    """Read cumulative Linux counters without instrumenting either packet path."""
    values = {"time": time.monotonic()}
    if os.name != "posix" or not Path("/proc/self/stat").is_file():
        return values
    for role, pid in (("server", server_pid), ("client", client_pid)):
        try:
            fields = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
            values[f"{role}_cpu_s"] = (int(fields[11]) + int(fields[12])) / os.sysconf("SC_CLK_TCK")
            for line in Path(f"/proc/{pid}/status").read_text().splitlines():
                if line.startswith("VmHWM:"):
                    values[f"{role}_peak_rss_mib"] = int(line.split()[1]) / 1024
        except FileNotFoundError:
            pass
    loopback_bytes = Path("/sys/class/net/lo/statistics/tx_bytes")
    if loopback_bytes.exists():
        values["loopback_bytes"] = int(loopback_bytes.read_text())
    snmp = Path("/proc/net/snmp")
    if snmp.exists():
        udp = [line.split()[1:] for line in snmp.read_text().splitlines() if line.startswith("Udp:")]
        if len(udp) == 2:
            counters = dict(zip(*udp))
            values["host_udp_rcvbuf_errors"] = int(counters["RcvbufErrors"])
    cpu_stat = Path("/sys/fs/cgroup/cpu.stat")
    if cpu_stat.is_file():
        quota = dict(line.split() for line in cpu_stat.read_text().splitlines())
        for name, label in CPU_QUOTA_COUNTERS.items():
            if name in quota:
                values[label] = int(quota[name])
    return values


def host_interval(start, end):
    elapsed = end["time"] - start["time"]
    detail = {"wall_s": round(elapsed, 3)}
    for role in ("server", "client"):
        key = f"{role}_cpu_s"
        if key in start and key in end and elapsed > 0:
            detail[f"{role}_mean_cpu_cores"] = round((end[key] - start[key]) / elapsed, 3)
    for key in ("loopback_bytes", "host_udp_rcvbuf_errors", *CPU_QUOTA_COUNTERS.values()):
        if key in start and key in end:
            detail[key] = end[key] - start[key]
    return detail


parser = argparse.ArgumentParser()
parser.add_argument("--server", type=Path, required=True)
parser.add_argument("--wine", type=Path)
parser.add_argument("--world", type=Path, required=True, help="stopped test world to copy")
parser.add_argument("--arena", nargs=3, type=int, metavar=("X","Y","Z"), default=(0,-59,0), help="arena centre at player foot height")
parser.add_argument("--client", type=Path, required=True, help="built cmd/burst executable")
parser.add_argument("--rpg", type=Path, required=True)
parser.add_argument("--players", type=int, default=100)
parser.add_argument("--play", type=int, default=60)
parser.add_argument("--pigs", type=int, default=64)
parser.add_argument("--port", type=int, default=19132)
parser.add_argument("--rpg-smoke", action="store_true")
parser.add_argument("--terrain-smoke", action="store_true")
parser.add_argument("--required-pack", action="store_true", help="require an uncached Bedrock model, sound and texture pack with 2.3 MB of incompressible transfer data")
parser.add_argument("--distant-terrain", action="store_true", help="include the distant terrain models and require each player to see a tile")
parser.add_argument("--require-budgets", action="store_true", help="exit with status 2 if any measured latency or delivery budget is missed")
parser.add_argument("--encounter", action="store_true", help="grant XP, damage every player and verify a synchronized heal")
parser.add_argument("--threshold", type=int, default=256)
parser.add_argument("--compression-level", type=int, choices=range(10), default=4)
parser.add_argument("--output", type=Path, required=True)
args = parser.parse_args()
if args.distant_terrain and not args.required_pack:
    parser.error("--distant-terrain needs --required-pack")
if not 2 <= args.players <= 250 or args.play < 4 or not 0 <= args.pigs <= 256:
    parser.error("players 2–250, play at least four seconds, pigs 0–256")
cx, cy, cz = args.arena

def position(x, y, z):
    return f"{cx+x} {cy+y} {cz+z}"

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
        ("networking.bedrock.compression","level"): str(args.compression_level),
        ("networking.lan_broadcast","enabled"): "false", ("networking.rcon","enabled"): "false",
    })
    pack_test_bytes = 2_304_000
    if args.required_pack:
        fixture = Path(__file__).resolve().parents[2] / "crates/sculith-packs/tests/fixtures"
        shutil.copytree(fixture / "pack", root / "source")
        shutil.copy2(fixture / "textures.json", root / "mapping.json")
        if args.distant_terrain:
            assets = Path(__file__).resolve().parents[2] / "examples/distant-terrain"
            for src in (assets / "pack").rglob("*"):
                if src.is_file() and src.name != "manifest.json":
                    dest = root / "source" / src.relative_to(assets / "pack")
                    dest.parent.mkdir(parents=True, exist_ok=True)
                    shutil.copyfile(src, dest)
            mapping_path = root / "mapping.json"
            mapping = json.loads(mapping_path.read_text())
            mapping["models"].extend(json.loads((assets / "mapping.json").read_text())["models"])
            mapping_path.write_text(json.dumps(mapping))
        (root / "source/credits.bin").write_bytes(hashlib.shake_256(b"Sculith pack burst").digest(pack_test_bytes))
        with cfg.open("a") as config:
            config.write('\n[resource_pack.shared]\nsource="source"\nmapping="mapping.json"\nrequired=true\n')
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
                    if line.startswith("ARENA_BUILDER_READY"):
                        match = re.search(r"threshold=(-?\d+)", line)
                        advertised_threshold = int(match[1]) if match else None
                        break
                else: raise RuntimeError(f"Arena preparation client failed. See {setup_log.name}")
            command("gamemode creative ArenaBuilder")
            if tuple(args.arena) != (0,-59,0):
                command(f"tp ArenaBuilder {position(0,4,0)}")
            command(f"forceload add {cx-16} {cz-16} {cx+16} {cz+16}")
            deadline=time.monotonic()+50
            while not loaded.is_set() and time.monotonic()<deadline:
                command(f"execute if loaded {position(-16,-1,-16)} if loaded {position(16,-1,16)} run time query daytime")
                loaded.wait(0.3)
            if not loaded.is_set():
                raise RuntimeError(f"Test arena chunks did not load. See {log_path}")
            for line in [
                "gamerule minecraft:spawn_mobs false", "gamerule minecraft:natural_health_regeneration false", "gamerule minecraft:respawn_radius 0",
                "gamerule minecraft:max_entity_cramming 0", "gamerule minecraft:advance_time false",
                f"fill {position(-16,-1,-16)} {position(16,-1,16)} minecraft:stone",
                f"fill {position(-16,0,-16)} {position(16,4,16)} minecraft:air", f"setworldspawn {position(0,0,0)}",
                f"fill {position(-16,0,-16)} {position(16,0,-16)} minecraft:stone", f"fill {position(-16,0,16)} {position(16,0,16)} minecraft:stone",
                f"fill {position(-16,0,-15)} {position(-16,0,15)} minecraft:stone", f"fill {position(16,0,-15)} {position(16,0,15)} minecraft:stone",
            ]:
                command(line)
            for i in range(args.pigs):
                command(f"summon minecraft:pig {position(i%8-4,0,i//8-4)}")
            command("list")
            wait(built,90)
            loaded.clear()
            command(f"execute if block {position(0,-1,0)} minecraft:stone if block {position(0,0,0)} minecraft:air if block {position(-16,0,-16)} minecraft:stone if block {position(16,0,16)} minecraft:stone run time query daytime")
            wait(loaded, 20)
            if args.distant_terrain:
                # Visit a 2x2 patch in each LOD band before the simultaneous login.
                # Build all three with ordinary chunks, then let the preparation client leave.
                for offset in (23, 34, 50):
                    chunk_x = cx // 16 + offset
                    chunk_z = cz // 16
                    x, z = chunk_x * 16, chunk_z * 16
                    loaded.clear()
                    command(f"tp ArenaBuilder {x+8} {cy+8} {z+8}")
                    deadline = time.monotonic() + 60
                    while not loaded.is_set() and time.monotonic() < deadline:
                        command(f"execute if loaded {x} {cy} {z} if loaded {x+31} {cy} {z+31} run time query daytime")
                        loaded.wait(0.3)
                    if not loaded.is_set():
                        raise RuntimeError(f"Distant terrain at {x}/{z} did not load")
                command("save-all flush")
                built.clear()
                command("list")
                wait(built, 90)
            builder.stdin.close()
            builder.wait(timeout=15)
            if args.terrain_smoke:
                with subprocess.Popen([str(args.client.resolve()), "-address", f"127.0.0.1:{args.port}", "-terrain-smoke"],
                                      stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True) as travel:
                    with out.with_suffix(".travel.log").open("w") as trace:
                        for line in travel.stdout:
                            trace.write(line)
                            trace.flush()
                            if line.startswith("TERRAIN_TELEPORT "):
                                _, chunk_x, chunk_z = line.split()
                                command(f"tp TerrainProbe {int(chunk_x)*16+0.5} {cy} {int(chunk_z)*16+0.5}")
                    if travel.wait():
                        raise RuntimeError(f"Bedrock terrain travel failed. See {out.with_suffix('.travel.log')}")
            if args.rpg_smoke:
                check = subprocess.Popen([str(args.client.resolve()),"-address",f"127.0.0.1:{args.port}","-rpg-smoke"],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.STDOUT,text=True)
                try:
                    with out.with_suffix(".rpg.log").open("w") as check_log:
                        for line in check.stdout:
                            check_log.write(line)
                            check_log.flush()
                            if line.startswith("RPG_AWARD"):
                                command("gamerule minecraft:natural_health_regeneration false")
                                command("experience add RpgProbe 105 points")
                                command("damage RpgProbe 6")
                                built.clear()
                                command("list")
                                wait(built,5)
                                check.stdin.write("applied\n")
                                check.stdin.flush()
                    if check.wait(timeout=10):
                        raise RuntimeError(f"RPG check failed. See {out.with_suffix('.rpg.log')}")
                finally:
                    if check.poll() is None: check.kill();check.wait()
            time.sleep(1)
            def tick_samples():
                while not fin.wait(1):
                    command("sculith performance")
            meter=threading.Thread(target=tick_samples,daemon=True)
            phase="join"
            meter.start()
            client=subprocess.Popen([str(args.client.resolve()),"-address",f"127.0.0.1:{args.port}","-players",str(args.players),
                "-marker-x",str(cx+3),"-marker-y",str(cy),"-marker-z",str(cz+4),
                "-output",str(out),"-play",f"{args.play}s"]+(["-encounter"] if args.encounter else []),stdout=subprocess.PIPE,stderr=subprocess.STDOUT,text=True)
            counters = {}
            try:
                with out.with_suffix(".clients.log").open("w") as client_log:
                    for line in client.stdout:
                        client_log.write(line)
                        client_log.flush()
                        if line.strip() == "JOIN" or line.startswith("PLAY ") or line.strip() == "FINISHED":
                            counters[line.split()[0]] = host_counters(server.pid, client.pid)
                        if line.startswith("PLAY "): phase="play"
                        if line.startswith("MARK "):
                            wave=int(line.split()[1])
                            command(f"setblock {position(3+wave,0,4)} minecraft:gold_block")
                            if args.encounter and wave==1:
                                command("experience add @a 105 points")
                                command("execute as @a run damage @s 4")
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
            report["advertised_compression_threshold"] = advertised_threshold
            report["compression_level"] = args.compression_level
            cpu_max=Path("/sys/fs/cgroup/cpu.max")
            if os.name == "posix" and cpu_max.is_file(): report["cohost_cpu_max"]=cpu_max.read_text().strip()
            report["clients_and_server_on_same_host"]=True
            report["world_preparation"]="World copy with one preparation client disconnected before the simultaneous start."
            report["arena_centre"] = [cx, cy, cz]
            report["input_hz"]=20
            report["online_authentication"]=False
            report["rpg_encounter_enabled"]=args.encounter
            report["resource_pack_required"] = args.required_pack
            if args.required_pack:
                report["incompressible_pack_test_bytes"] = pack_test_bytes
            report["host_intervals"] = {
                label: host_interval(counters[start], counters[end])
                for label, start, end in (("join", "JOIN", "PLAY"), ("play", "PLAY", "FINISHED"))
                if start in counters and end in counters
            }
            report["peak_rss_mib"] = {
                role: max(values[f"{role}_peak_rss_mib"] for values in counters.values() if f"{role}_peak_rss_mib" in values)
                for role in ("server", "client") if any(f"{role}_peak_rss_mib" in values for values in counters.values())
            }
            report["tick_summary"]={
                label: max((sample["p95_ms"] for sample in samples if sample["phase"]==label),default=None)
                for label in ("join","play")
            }
            report["checks"]={
                "compression_threshold_negotiated": advertised_threshold == min(args.threshold, 65535),
                "everyone_spawned_and_alive": report["spawned"]==args.players==report["survived"],
                "everyone_saw_a_measured_peer": report["viewers_with_matched_peer"]==args.players,
                "everyone_granted_radius_16": report["clients_granted_radius_16"]==args.players,
                "everyone_had_peer_skin_before_spawn": report["known_peer_before_actor_spawn"]==args.players,
                "every_client_sent_at_least_95_percent_of_inputs": report["minimum_input_rate_percent"]>=95,
                "every_viewer_matched_at_least_95_percent_of_peer_inputs": report.get("minimum_matched_peer_inputs_percent",0)>=95,
                "no_viewer_motion_gap_above_250_ms": report["per_viewer_max_motion_gap"].get("max_ms",float("inf"))<=250,
                "everyone_saw_three_world_edits": report["viewers_receiving_three_markers"]==args.players,
                "everyone_saw_a_mob": args.pigs==0 or report["viewers_receiving_pigs"]==args.players,
                "everyone_received_rpg_reply": report["rpg_replies"]==args.players,
                "tick_p95_within_50_ms_during_join_and_play": all(v is not None and v<=50 for v in report["tick_summary"].values()),
                "remote_motion_p95_within_150_ms_for_every_viewer": report["per_viewer_motion_p95"].get("max_ms",float("inf"))<=150,
                "world_edits_p95_within_150_ms": report["world_block_to_visible_update"].get("p95_ms",float("inf"))<=150,
            }
            if args.encounter:
                report["checks"]["everyone_healed"]=report["rpg_heal_feedback"]["samples"]==args.players
                report["checks"]["rpg_heal_p95_within_150_ms"]=report["rpg_heal_feedback"].get("p95_ms",float("inf"))<=150
            if args.required_pack:
                report["checks"]["everyone_received_the_same_full_pack"] = (
                    list(report["resource_pack_copies_by_sha256"].values()) == [args.players]
                    and report["resource_pack_bytes_received"] >= args.players * pack_test_bytes
                )
            if args.distant_terrain:
                report["checks"]["every_viewer_received_distant_terrain"] = report.get("viewers_receiving_lod_tiles", 0) == args.players
                report["checks"]["each_viewer_received_three_terrain_scales"] = report.get("viewers_receiving_all_lod_scales") == args.players
                report["checks"]["lod_actor_order_and_budget"] = report.get("lod_invalid_sequences", -1) == 0 and 0 < report.get("lod_peak_per_viewer", 0) <= 32
            if coverage := report.get("initial_terrain_coverage"):
                report["checks"]["initial_view_delivered_during_workload"] = coverage["viewers_receiving_every_position"] == args.players
            report["all_budgets_met"]=all(report["checks"].values())
            out.write_text(json.dumps(report,indent=2)+"\n")
            print(json.dumps({k:v for k,v in report.items() if k not in ("tick_windows","client_errors")},ensure_ascii=False), flush=True)
            functional_checks = [value for key, value in report["checks"].items()
                                 if key.startswith("everyone_") or key == "compression_threshold_negotiated"]
            if result or not all(functional_checks):
                raise SystemExit(1)
            if args.require_budgets and not report["all_budgets_met"]:
                raise SystemExit(2)
        finally:
            fin.set()
            if builder is not None and builder.poll() is None:
                builder.kill()
                builder.wait()
            stop_server(server)
            reader.join(timeout=10)
