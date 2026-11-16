#!/usr/bin/env python3
"""Independent Java/native membership, pose, late-join and leave verification."""
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
from plugin_game_smoke import nbt_text

ROOT=Path(__file__).resolve().parents[2]

def until(client, predicate):
    deadline=time.monotonic()+10
    while not predicate():
        assert time.monotonic()<deadline,"peer state deadline"
        id_,reader=wire.receive(client.socket)
        if id_==124:
            if not hasattr(client,'chat_messages'):client.chat_messages=[]
            client.chat_messages.append(nbt_text(reader));assert reader.number('>B')==0;reader.end();continue
        assert wire.consume_peer(client,id_,reader),f"unexpected public peer packet {id_}"

def smoke(server,native,plugin=None):
    with tempfile.TemporaryDirectory(prefix="sculith peers ") as temporary:
        root=Path(temporary);log=root/"server.log";settings=root/"server.toml"
        settings.write_text('[java]\nsessions=true\nrequire_authentication=false\n'
            '[bedrock]\nsessions=true\nrequire_authentication=false\n'
            'nethernet_identity_path='+json.dumps(str(root/"identity.pem"))+'\n'
            '[network.java]\nbind="127.0.0.1:0"\n[network.bedrock]\nbind="127.0.0.1:0"\n',encoding="utf-8")
        if plugin:
            with settings.open('a',encoding='utf-8') as config:
                config.write('[[plugins]]\npath='+json.dumps(str(plugin))+'\nargs=["--trace"]\n')
                config.write('[[plugins]]\npath='+json.dumps(str(plugin))+'\nargs=["--observe"]\n')
        with log.open("w",encoding="utf-8") as output:
            process=subprocess.Popen([str(server),"run","--config",str(settings)],stdout=output,stderr=output,cwd=ROOT,
                env=dict(os.environ,NO_COLOR="1",RUST_LOG="info,sculith::protocol::bedrock=debug"),creationflags=subprocess.CREATE_NEW_PROCESS_GROUP if os.name=="nt" else 0)
            java=late=probe=None;lines=[]
            try:
                deadline=time.monotonic()+8
                while time.monotonic()<deadline:
                    text=log.read_text(encoding="utf-8")
                    tcp=re.search(r'Java listening.*?127\.0\.0\.1:(\d+)',text)
                    bedrock=re.search(r'NetherNet listening.*?127\.0\.0\.1:(\d+)',text)
                    if tcp and bedrock:break
                    assert process.poll() is None,text
                    time.sleep(.05)
                else:raise TimeoutError(text)
                address=("127.0.0.1",int(tcp[1]))
                java=wire.Client(address,"PeerJavaProbe")
                until(java,lambda:hasattr(java,"peer_profiles") and java.uuid.bytes in java.peer_profiles)
                assert java.entity_id==1 and not java.peer_entities
                probe=subprocess.Popen([str(native),"http://127.0.0.1:"+bedrock[1],("--events" if plugin else "--peers")],stdout=subprocess.PIPE,stderr=subprocess.STDOUT,text=True,encoding="utf-8")
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
                barrier("PEER_READY")
                if plugin:
                    assert set(java.commands)=={'help','hello'}
                    wire.send(java.socket,0x16,wire.string('sculith:metadata_probe')+b'SCULITH_SECRET_PACKET_SENTINEL')
                    wire.send(java.socket,0x36,struct.pack('>h',1))
                    wire.send(java.socket,9,wire.string('java 반가워 😀')+struct.pack('>qq',int(time.time()*1000),0)+bytes(6))
                    wire.send(java.socket,7,wire.string('hello SCULITH_SECRET_COMMAND_SENTINEL'))
                until(java,lambda:2 in java.peer_entities and java.peer_entities[2]["position"]==[.5,4,2])
                if plugin:
                    until(java,lambda:len(getattr(java,'chat_messages',[]))==3)
                    assert set(java.chat_messages)=={'<베드락😀> native 안녕 😀','<PeerJavaProbe> java 반가워 😀','Hello PeerJavaProbe 👋'}
                native_uuid=java.peer_entities[2]['uuid']
                assert java.peer_profiles[native_uuid]=={'name':'Scu_2','display':'베드락😀'}
                wire.send(java.socket,0x1f,struct.pack(">dddffB",1.5,4,.5,90,0,1))
                barrier("PEER_JAVA_MOVED")
                late=wire.Client(address,"PeerLateProbe")
                until(late,lambda:hasattr(late,"peer_entities") and set(late.peer_entities)=={1,2})
                assert late.entity_id==3 and late.peer_entities[1]['position']==[1.5,4,.5]
                assert late.peer_entities[2]['position']==[.5,4,2]
                assert late.peer_entities[2]['uuid']==native_uuid
                barrier("PEER_LATE_JOINED")
                late_uuid=late.uuid.bytes
                until(java,lambda:3 in java.peer_entities)
                late.socket.close();late=None
                assert probe.wait(timeout=10)==0
                reader.join(timeout=2)
                assert "PASS: independent Bedrock peer roster" in "".join(lines),"".join(lines)
                until(java,lambda:not java.peer_entities and native_uuid not in java.peer_profiles and late_uuid not in java.peer_profiles)
                assert set(java.peer_profiles)=={java.uuid.bytes}
                java.socket.close();java=None
                process.send_signal(signal.CTRL_BREAK_EVENT if os.name=="nt" else signal.SIGINT)
                assert process.wait(timeout=5)==0
                text=log.read_text(encoding="utf-8")
                assert 'peer event backlog' not in text
                if plugin:
                    assert 'events-demo' in text and 'api_version=6' in text
                    assert re.search(r'events packet .*edition=Java .*name=minecraft:custom_payload dispatch=Unsupported',text)
                    assert re.search(r'events packet .*edition=Bedrock .*name=ScriptCustomEvent dispatch=Unsupported',text)
                    assert len(re.findall(r'events move player=',text))>=2
                    assert len(re.findall(r'events equipment player=',text))>=2
                    assert 'SCULITH_SECRET_PACKET_SENTINEL' not in text and 'plugin disabled' not in text
                    assert 'events plugin stopped' in text
                    assert len(re.findall(r'events chat player=',text))==2
                    assert len(re.findall(r'events command player=',text))==2
                    assert 'SCULITH_SECRET_COMMAND_SENTINEL' not in text
                    assert 'unexpected foreign command invocation' not in text
                print("".join(lines),end="")
            except Exception:
                print(log.read_text(encoding="utf-8"));print("".join(lines));raise
            finally:
                for client in (java,late):
                    if client:client.socket.close()
                if probe and probe.poll() is None:probe.kill();probe.wait(timeout=5)
                if process.poll() is None:process.kill();process.wait(timeout=5)
    print("PASS: independent Java/native initialized membership, eye/feet coordinates, both movement directions, current late-join snapshot and ordered entity/roster cleanup")

if __name__=="__main__":
    parser=argparse.ArgumentParser(description=__doc__);suffix=".exe" if os.name=="nt" else ""
    parser.add_argument("--server",type=Path,default=ROOT/("target/debug/sculith"+suffix))
    parser.add_argument("--client",type=Path,required=True)
    args=parser.parse_args();smoke(args.server.resolve(strict=True),args.client.resolve(strict=True))
