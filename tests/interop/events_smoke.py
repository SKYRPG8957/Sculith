#!/usr/bin/env python3
"""Independent complete identifier registration and live Rust API-v6 hooks."""
import argparse
import json
import os
from pathlib import Path
import subprocess
from peer_smoke import smoke

ROOT=Path(__file__).resolve().parents[2]
if __name__=="__main__":
    parser=argparse.ArgumentParser(description=__doc__);suffix=".exe" if os.name=="nt" else ""
    parser.add_argument("--server",type=Path,default=ROOT/("target/debug/sculith"+suffix))
    parser.add_argument("--plugin",type=Path,default=ROOT/("target/debug/sculith-events-plugin"+suffix))
    parser.add_argument("--client",type=Path,required=True)
    parser.add_argument("--api-version",type=int,choices=(6,7,8,9,10,11),default=11)
    args=parser.parse_args();server=args.server.resolve(strict=True)
    entries=json.loads(subprocess.check_output([str(server),'packets'],text=True,encoding='utf-8'))
    java=[p for p in entries if p['edition']=='java'];native=[p for p in entries if p['edition']=='bedrock']
    facts=json.loads((ROOT/'crates/sculith-server/src/protocol/java/protocol777.json').read_text())
    expected={(phase,direction,id_,name) for phase,directions in facts['packets'].items() for direction,packets in directions.items() for name,id_ in packets.items()}
    assert len(java)==260 and {(p['phase'],p['direction'],p['id'],p['name']) for p in java}==expected
    reference=json.loads((ROOT/'crates/sculith-server/src/protocol/bedrock/packet_pool2225.json').read_text())
    assert len(native)==244 and {p['id'] for p in native}=={p['id'] for p in reference['packets']}
    live_reference=json.loads(subprocess.check_output([str(args.client.resolve(strict=True)),"--packet-catalog"],text=True,encoding="utf-8"))
    assert live_reference==reference['packets'],"pinned live Go packet pools differ from checked-in facts"
    assert {p['id'] for p in native}=={p['id'] for p in live_reference}
    assert sum(p['typed_codec'] for p in native)==239 and len({p['id'] for p in native})==244
    smoke(server,args.client.resolve(strict=True),args.plugin.resolve(strict=True),args.api_version)
    print('PASS: 260 Java and 244 native identifiers, API-v6 packet/move/equipment/chat hooks, public command catalogs, private Rust commands, origin rejection, gap detection and body redaction')
