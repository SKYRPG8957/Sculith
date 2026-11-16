#!/usr/bin/env python3
"""Independent required-pack and private static-model lifecycle checks."""
import argparse
import hashlib
import io
import json
import os
from pathlib import Path
import re
import signal
import subprocess
import tempfile
import time
from urllib.request import urlopen
import zipfile
import java_smoke as wire

ROOT=Path(__file__).resolve().parents[1]

def receive(client):
    while True:
        id_,reader=wire.receive_play(client)
        if id_==45:
            wire.send(client.socket,28,reader.take(8));reader.end()
        else:return id_,reader

def removed(client):
    id_,r=receive(client);assert id_==78 and r.varint()==1
    entity=r.varint();r.end();return entity

def shown(client,entity,previous_uuid=None):
    assert removed(client)==entity
    id_,r=receive(client);assert id_==1 and r.varint()==entity
    uuid=r.take(16)
    if previous_uuid is not None:assert uuid==previous_uuid
    assert r.varint()==73
    assert [r.number(">d") for _ in range(3)]==[.5,4.5,3.5]
    assert r.take(4)==bytes(4) and r.varint()==0;r.end()
    id_,r=receive(client);assert id_==101 and r.varint()==entity
    assert r.take(5)==bytes([5,8,1,23,7])
    assert [r.varint() for _ in range(5)]==[1,1,1,0,10]
    assert r.string()=="sculith:demo_statue" and r.take(4)==bytes([24,0,0,255]);r.end()
    return uuid

def accept_pack(reader,sock):
    id_=reader.take(16);url=reader.string();sha=reader.string()
    assert reader.take(2)==b"\x01\x00"
    wire.send(sock,6,id_+wire.vi(3))
    with urlopen(url,timeout=3) as response:archive=response.read(32*1024*1024+1)
    assert hashlib.sha1(archive).hexdigest()==sha
    with zipfile.ZipFile(io.BytesIO(archive)) as pack:
        assert pack.testzip() is None
        texture=pack.read("assets/sculith/textures/item/sculith_models/demo_statue.png")
        assert texture==(ROOT/"examples/pack/bedrock/textures/entity/sculith_statue.png").read_bytes()
        definition=json.loads(pack.read("assets/sculith/items/demo_statue.json"))
        assert definition=={"model":{"type":"minecraft:model","model":"sculith:sculith/demo_statue"}}
        model=json.loads(pack.read("assets/sculith/models/sculith/demo_statue.json"))
        assert model["textures"]=={"main":"sculith:item/sculith_models/demo_statue","particle":"sculith:item/sculith_models/demo_statue"}
        original=json.loads((ROOT/"examples/pack/bedrock/models/entity/sculith_statue.geo.json").read_text())
        cubes=[cube for bone in original["minecraft:geometry"][0]["bones"] for cube in bone.get("cubes",[])]
        assert len(cubes)==len(model["elements"])==3
        for cube,element in zip(cubes,model["elements"]):
            expected=[cube["origin"][0]+8,cube["origin"][1],cube["origin"][2]+8]
            assert element["from"]==expected and element["to"]==[a+b for a,b in zip(expected,cube["size"])]
            assert set(element["faces"])==set(cube["uv"])
            assert all(face=={"uv":[0,0,16,16],"texture":"#main"} for face in element["faces"].values())
    wire.send(sock,6,id_+wire.vi(4));wire.send(sock,6,id_+wire.vi(0))

def smoke(server,plugin,client,api_version=6):
    with tempfile.TemporaryDirectory(prefix="sculith model ") as temporary:
        root=Path(temporary);log=root/"server.log";settings=root/"server.toml"
        settings.write_text('[java]\nsessions=true\nrequire_authentication=false\n'
                            '[bedrock]\nsessions=true\nrequire_authentication=false\n'
                            'nethernet_identity_path='+json.dumps(str(root/"identity.pem"))+'\n'
                            '[network.java]\nbind="127.0.0.1:0"\n[network.bedrock]\nbind="127.0.0.1:0"\n'
                            '[content]\nsource='+json.dumps(str(ROOT/"examples/pack/bedrock"))+'\n'
                            'mapping='+json.dumps(str(ROOT/"examples/pack/textures.json"))+'\n'
                            '[[plugins]]\npath='+json.dumps(str(plugin))+'\nargs=["--exercise"]\n',encoding="utf-8")
        with log.open("w",encoding="utf-8") as output:
            process=subprocess.Popen([str(server),"run","--config",str(settings)],stdout=output,stderr=output,cwd=ROOT,
                env=dict(os.environ,NO_COLOR="1",RUST_LOG="info"),creationflags=subprocess.CREATE_NEW_PROCESS_GROUP if os.name=="nt" else 0)
            java=None
            try:
                deadline=time.monotonic()+8
                while time.monotonic()<deadline:
                    text=log.read_text(encoding="utf-8")
                    tcp=re.search(r'Java TCP discovery listening.*?127\.0\.0\.1:(\d+)',text)
                    native=re.search(r'NetherNet HTTP signaling listening.*?127\.0\.0\.1:(\d+)',text)
                    if tcp and native:break
                    assert process.poll() is None,text
                    time.sleep(.05)
                else:raise TimeoutError(text)
                assert "model-demo" in text and ("api_version="+str(api_version)) in text
                java=wire.Client(("127.0.0.1",int(tcp[1])),"ModelJavaProbe",pack_handler=accept_pack)
                uuid=shown(java,2)
                native_result=subprocess.run([str(client),"http://127.0.0.1:"+native[1],"--model-output",str(root/"native-model.mcpack")],capture_output=True,text=True,encoding="utf-8",timeout=40)
                assert native_result.returncode==0,native_result.stdout+native_result.stderr
                assert "PASS: independent Bedrock registered static model" in native_result.stdout
                with zipfile.ZipFile(root/"native-model.mcpack") as native_pack:
                    assert native_pack.testzip() is None
                    for asset in ("entity/sculith_statue.entity.json","models/entity/sculith_statue.geo.json","textures/entity/sculith_statue.png","render_controllers/sculith_static.render_controllers.json"):
                        assert native_pack.read(asset)==(ROOT/"examples/pack/bedrock"/asset).read_bytes()
                # Native actor traffic must not enter Java's private lifecycle.
                shown(java,2,uuid);assert removed(java)==2
                wire.send(java.socket,0x29,wire.vi(0)+wire.pos(1,3,1)+b"\x01"+wire.vi(1))
                java.block((1,3,1),0,"air")
                java.socket.close();java=None
                process.send_signal(signal.CTRL_BREAK_EVENT if os.name=="nt" else signal.SIGINT)
                assert process.wait(timeout=5)==0
                text=log.read_text(encoding="utf-8")
                assert "model plugin stopped" in text and "plugin model rejected" not in text and "plugin disabled" not in text
            except Exception:
                print(log.read_text(encoding="utf-8"));raise
            finally:
                if java:java.socket.close()
                if process.poll() is None:process.kill();process.wait(timeout=5)
    print("PASS: real Rust API-v5 shared static model, original geometry/texture, independent Java/native spawn/replacement/hide, actor ID and private-recipient isolation")

if __name__=="__main__":
    parser=argparse.ArgumentParser(description=__doc__);suffix=".exe" if os.name=="nt" else ""
    parser.add_argument("--server",type=Path,default=ROOT/("target/debug/sculith"+suffix))
    parser.add_argument("--plugin",type=Path,default=ROOT/("target/debug/sculith-model-plugin"+suffix))
    parser.add_argument("--client",type=Path,required=True)
    parser.add_argument("--api-version",type=int,choices=(5,6),default=6)
    args=parser.parse_args();smoke(args.server.resolve(strict=True),args.plugin.resolve(strict=True),args.client.resolve(strict=True),args.api_version)
