#!/usr/bin/env python3
"""Independent authoritative held-item projection across Java and Bedrock."""
import argparse
import json
import os
from pathlib import Path
import queue
import re
import signal
import struct
import subprocess
import tempfile
import threading
import time
import java_smoke as wire

ROOT=Path(__file__).resolve().parents[1]

def until(client, predicate):
    deadline=time.monotonic()+10
    while not predicate():
        assert time.monotonic()<deadline,"peer state deadline"
        id_,reader=wire.receive(client.socket)
        assert wire.consume_peer(client,id_,reader),f"unexpected public peer packet {id_}"

def smoke(server,native):
    with tempfile.TemporaryDirectory(prefix="sculith equipment ") as temporary:
        root=Path(temporary);log=root/"server.log";settings=root/"server.toml"
        settings.write_text('[java]\nsessions=true\nrequire_authentication=false\n'
            '[bedrock]\nsessions=true\nrequire_authentication=false\n'
            'nethernet_identity_path='+json.dumps(str(root/"identity.pem"))+'\n'
            '[network.java]\nbind="127.0.0.1:0"\n[network.bedrock]\nbind="127.0.0.1:0"\n',encoding="utf-8")
        with log.open("w",encoding="utf-8") as output:
            process=subprocess.Popen([str(server),"run","--config",str(settings)],stdout=output,stderr=output,cwd=ROOT,
                env=dict(os.environ,NO_COLOR="1",RUST_LOG="info,sculith::protocol::bedrock=debug"),creationflags=subprocess.CREATE_NEW_PROCESS_GROUP if os.name=="nt" else 0)
            java=late=probe=None;lines=[]
            try:
                deadline=time.monotonic()+8
                while time.monotonic()<deadline:
                    text=log.read_text(encoding="utf-8")
                    tcp=re.search(r'Java TCP discovery listening.*?127\.0\.0\.1:(\d+)',text)
                    bedrock=re.search(r'NetherNet HTTP signaling listening.*?127\.0\.0\.1:(\d+)',text)
                    if tcp and bedrock:break
                    assert process.poll() is None,text
                    time.sleep(.05)
                else:raise TimeoutError(text)
                address=("127.0.0.1",int(tcp[1]))
                java=wire.Client(address,"PeerJavaProbe")
                until(java,lambda:hasattr(java,"peer_profiles") and java.uuid.bytes in java.peer_profiles)
                assert java.entity_id==1 and not java.peer_entities
                probe=subprocess.Popen([str(native),"http://127.0.0.1:"+bedrock[1],"--equipment"],stdout=subprocess.PIPE,stderr=subprocess.STDOUT,text=True,encoding="utf-8")
                lines=[];updates=queue.Queue()
                def read_output():
                    for line in probe.stdout:lines.append(line);updates.put(line.strip())
                    updates.put(None)
                reader=threading.Thread(target=read_output,daemon=True);reader.start()
                def barrier(marker):
                    deadline=time.monotonic()+20
                    while time.monotonic()<deadline:
                        line=updates.get(timeout=max(.01,deadline-time.monotonic()))
                        assert line is not None,"".join(lines)
                        if line==marker:return
                    raise TimeoutError("".join(lines))
                barrier("EQUIPMENT_READY")
                until(java,lambda:2 in java.peer_entities and java.peer_entities[2].get('held')==(64,1))
                native_uuid=java.peer_entities[2]['uuid']
                wire.send(java.socket,0x36,struct.pack(">h",1))
                barrier("EQUIPMENT_JAVA_DIRT");barrier("EQUIPMENT_NATIVE_GRASS")
                until(java,lambda:java.peer_entities[2].get('held')==(64,54))
                late=wire.Client(address,"PeerLateProbe")
                until(late,lambda:hasattr(late,"peer_entities") and set(late.peer_entities)=={1,2} and all('held' in p for p in late.peer_entities.values()))
                assert late.entity_id==3 and late.peer_entities[1]['held']==(64,55)
                assert late.peer_entities[2]['held']==(64,54)
                barrier("EQUIPMENT_LATE_JOINED")
                until(java,lambda:3 in java.peer_entities and java.peer_entities[3].get('held')==(64,1))
                wire.send(java.socket,0x36,struct.pack(">h",8))
                barrier("EQUIPMENT_JAVA_EMPTY");barrier("EQUIPMENT_NATIVE_EMPTY")
                until(java,lambda:java.peer_entities[2].get('held')==(0,0))
                until(late,lambda:late.peer_entities[1].get('held')==(0,0) and late.peer_entities[2].get('held')==(0,0))
                late_uuid=late.uuid.bytes;late.socket.close();late=None
                assert probe.wait(timeout=10)==0
                reader.join(timeout=2)
                assert "PASS: independent Bedrock initial held item" in "".join(lines),"".join(lines)
                until(java,lambda:not java.peer_entities and native_uuid not in java.peer_profiles and late_uuid not in java.peer_profiles)
                assert set(java.peer_profiles)=={java.uuid.bytes}
                java.socket.close();java=None
                process.send_signal(signal.CTRL_BREAK_EVENT if os.name=="nt" else signal.SIGINT)
                assert process.wait(timeout=5)==0
                assert 'peer event backlog' not in log.read_text(encoding="utf-8")
                print("".join(lines),end="")
            except Exception:
                print(log.read_text(encoding="utf-8"));print("".join(lines));raise
            finally:
                for client in (java,late):
                    if client:client.socket.close()
                if probe and probe.poll() is None:probe.kill();probe.wait(timeout=5)
                if process.poll() is None:process.kill();process.wait(timeout=5)
    print("PASS: independent Java/native initial held items, canonical selection despite forged item/count claims, current late-join equipment, empty hands and cleanup")

if __name__=="__main__":
    parser=argparse.ArgumentParser(description=__doc__);suffix=".exe" if os.name=="nt" else ""
    parser.add_argument("--server",type=Path,default=ROOT/("target/debug/sculith"+suffix))
    parser.add_argument("--client",type=Path,required=True)
    args=parser.parse_args();smoke(args.server.resolve(strict=True),args.client.resolve(strict=True))
