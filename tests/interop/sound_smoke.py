#!/usr/bin/env python3
"""Independent Java/native sound archives, wire effects and recipient isolation."""
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

ROOT = Path(__file__).resolve().parents[2]

def smoke(server, plugin, client, api_version=11):
    with tempfile.TemporaryDirectory(prefix="sculith sound ") as temporary:
        root=Path(temporary); log=root/"server.log"; settings=root/"server.toml"
        settings.write_text('[java]\nsessions=true\nrequire_authentication=false\n'
                            '[bedrock]\nsessions=true\nrequire_authentication=false\n'
                            'nethernet_identity_path='+json.dumps(str(root/"identity.pem"))+'\n'
                            '[network.java]\nbind="127.0.0.1:0"\n[network.bedrock]\nbind="127.0.0.1:0"\n'
                            '[content]\nsource='+json.dumps(str(ROOT/"examples/pack/bedrock"))+'\n'
                            'mapping='+json.dumps(str(ROOT/"examples/pack/textures.json"))+'\n'
                            '[[plugins]]\npath='+json.dumps(str(plugin))+'\n',encoding="utf-8")
        with log.open("w",encoding="utf-8") as output:
            process=subprocess.Popen([str(server),"run","--config",str(settings)],stdout=output,stderr=output,cwd=ROOT,
                env=dict(os.environ,NO_COLOR="1",RUST_LOG="info"),creationflags=subprocess.CREATE_NEW_PROCESS_GROUP if os.name=="nt" else 0)
            java=None
            try:
                deadline=time.monotonic()+8
                while time.monotonic()<deadline:
                    text=log.read_text(encoding="utf-8")
                    tcp=re.search(r'Java listening.*?127\.0\.0\.1:(\d+)',text)
                    native=re.search(r'NetherNet listening.*?127\.0\.0\.1:(\d+)',text)
                    if tcp and native: break
                    assert process.poll() is None,text
                    time.sleep(.05)
                else: raise TimeoutError(text)
                assert "sound-demo" in text and f"api_version={api_version}" in text
                loaded=[]
                def accept(reader,sock):
                    id_=reader.take(16);url=reader.string();sha=reader.string()
                    assert reader.take(2)==b"\x01\x00"
                    wire.send(sock,6,id_+wire.vi(3))
                    with urlopen(url,timeout=3) as response: archive=response.read(32*1024*1024+1)
                    assert hashlib.sha1(archive).hexdigest()==sha
                    with zipfile.ZipFile(io.BytesIO(archive)) as pack:
                        assert pack.testzip() is None
                        clip=pack.read("assets/sculith/sounds/demo_tone.ogg")
                        assert clip==(ROOT/"examples/pack/bedrock/sounds/sculith_demo.ogg").read_bytes()
                        assert json.loads(pack.read("assets/sculith/sounds.json"))=={"demo_tone":{"sounds":[{"name":"sculith:demo_tone","stream":False}]}}
                    loaded.append(True)
                    wire.send(sock,6,id_+wire.vi(4));wire.send(sock,6,id_+wire.vi(0))
                java=wire.Client(("127.0.0.1",int(tcp[1])),"SoundJavaProbe",pack_handler=accept)
                id_,reader=wire.receive_play(java)
                assert id_==119 and reader.varint()==0 and reader.string()=="sculith:demo_tone"
                assert reader.number(">B")==0 and reader.varint()==7
                assert [reader.number(">i") for _ in range(3)]==[4,44,4]
                assert abs(reader.number(">f")-.7)<1e-6 and reader.number(">f")==1
                assert reader.number(">q")==0;reader.end();assert len(loaded)==1
                native_result=subprocess.run([str(client),"http://127.0.0.1:"+native[1],"--sound"],capture_output=True,text=True,encoding="utf-8",timeout=40)
                assert native_result.returncode==0,native_result.stdout+native_result.stderr
                assert "PASS: independent Bedrock original OGG" in native_result.stdout
                # The Java block reader rejects an extra sound packet, detecting
                # accidental broadcast of the native player's private sound.
                wire.send(java.socket,0x29,wire.vi(0)+wire.pos(1,3,1)+b"\x01"+wire.vi(1))
                java.block((1,3,1),0,"air")
                java.socket.close();java=None
                process.send_signal(signal.CTRL_BREAK_EVENT if os.name=="nt" else signal.SIGINT)
                assert process.wait(timeout=5)==0
                text=log.read_text(encoding="utf-8")
                assert text.count("sound requested for player=")==2
                assert "sound plugin stopped" in text and "plugin sound rejected" not in text and "plugin disabled" not in text
            except Exception:
                print(log.read_text(encoding="utf-8"));raise
            finally:
                if java: java.socket.close()
                if process.poll() is None: process.kill();process.wait(timeout=5)
    print("PASS: real Rust API-v4 private shared sound, exact original OGG in both packs, independent Java/native wire decoding, recipient isolation and graceful stopping")

if __name__=="__main__":
    parser=argparse.ArgumentParser(description=__doc__);suffix=".exe" if os.name=="nt" else ""
    parser.add_argument("--server",type=Path,default=ROOT/("target/debug/sculith"+suffix))
    parser.add_argument("--plugin",type=Path,default=ROOT/("target/debug/sculith-sound-plugin"+suffix))
    parser.add_argument("--client",type=Path,required=True)
    parser.add_argument("--api-version",type=int,choices=(4,5,6,7,8,9,10,11),default=11)
    args=parser.parse_args()
    smoke(args.server.resolve(strict=True),args.plugin.resolve(strict=True),args.client.resolve(strict=True),args.api_version)
