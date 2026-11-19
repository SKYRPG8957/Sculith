"""Measure the packaged Bedrock server on the local Windows PC."""
import argparse
from datetime import datetime
import json
from pathlib import Path
import subprocess
import sys
import tempfile

base = Path(__file__).resolve().parent
p = argparse.ArgumentParser()
p.add_argument("server", nargs="?", type=Path)
p.add_argument("--quick", action="store_true", help="two-player installation check")
args = p.parse_args()
possible = [args.server] if args.server else [base.parent / "Sculith/sculith.exe", base / "sculith.exe"]
server = next((s.resolve() for s in possible if s and s.is_file()), None)
if server is None:
    sys.exit("Sculith 폴더와 Sculith-100-Test 폴더를 나란히 놓거나 sculith.exe를 run-100-test.cmd 위에 끌어다 놓으세요.")
plugin = server.parent / "plugins/sculith-rpg.wasm"
if not plugin.is_file():
    sys.exit("구동기 ZIP의 plugins/sculith-rpg.wasm 파일이 필요합니다.")
results = base / "results"
results.mkdir(exist_ok=True)
out = Path(tempfile.mkdtemp(prefix=datetime.now().strftime("%Y%m%d-%H%M%S-"), dir=results)) / "report.json"
players, seconds = (2, 8) if args.quick else (100, 60)
print(f"내 PC에서 {players}명 동시 접속, 16청크 시야, 64마리 몹, 필수 리소스팩과 RPG 회복을 시험합니다.", flush=True)
print(f"시험 결과 {out}", flush=True)
command = [sys.executable, "-X", "utf8", str(base / "tests/interop/bedrock_burst.py"),
           "--server", str(server), "--world", str(base / "test-world"),
           "--client", str(base / "bedrock-test-client.exe"), "--rpg", str(plugin),
           "--players", str(players), "--play", str(seconds), "--pigs", "64",
           "--encounter", "--required-pack", "--require-budgets", "--port", "19374",
           "--output", str(out)]
with (out.parent / "driver.log").open("w", encoding="utf-8") as log:
    status = subprocess.run(command, cwd=base, stdout=log, stderr=subprocess.STDOUT).returncode
if not out.is_file():
    sys.exit(f"시험이 끝나기 전에 구동기나 클라이언트가 종료되었습니다. {out.parent}의 로그를 확인하세요.")
data = json.loads(out.read_text(encoding="utf-8"))
print(f"입장 {data['spawned']}/{players}명, 종료 때 접속 {data['survived']}명")
print(f"플레이 중 틱 P95 최대값 {data['tick_summary']['play']}ms, 움직임 지연 P95 {data['peer_input_to_visible_motion'].get('p95_ms')}ms")
print("목표 달성" if data["all_budgets_met"] else "목표 미달")
print(out)
sys.exit(status)
