#!/usr/bin/env python3
"""Independent Java/native block picking without starter inventory or game commands."""
import argparse,json,os,re,signal,socket,struct,subprocess,tempfile,time,threading,queue
from pathlib import Path
import java_smoke as wire
from world_api_smoke import wait
ROOT=Path(__file__).resolve().parents[2]

def picked(java,selected,items):
    for _ in range(128):
        id_,reader=wire.receive_play(java)
        if id_==0x6e:
            slot=reader.varint();assert 0<=slot<41;wire.item_stack(reader);reader.end()
        elif id_==0x6b:
            assert reader.varint()==selected;reader.end()
            actual={i:stack for i,stack in enumerate(java.menu[36:45]) if stack[0]}
            assert actual==items,(actual,items);return
        else:raise AssertionError(('unexpected pick frame',id_))
    raise AssertionError('missing selected-slot correction')

def smoke(server,native,observer):
    with tempfile.TemporaryDirectory(prefix='sculith block pick ') as temporary:
        root=Path(temporary);settings=root/'server.toml';log=root/'server.log'
        settings.write_text('[java]\nsessions=true\nrequire_authentication=false\n[bedrock]\nsessions=true\nrequire_authentication=false\nnethernet_identity_path='+json.dumps(str(root/'identity.pem'))+'\n[network.java]\nbind="127.0.0.1:0"\n[network.bedrock]\nbind="127.0.0.1:0"\n[[plugins]]\npath='+json.dumps(str(observer))+'\nargs=["--trace"]\n',encoding='utf-8')
        with log.open('w',encoding='utf-8') as output:
            process=subprocess.Popen([str(server),'run','--config',str(settings)],stdout=output,stderr=output,cwd=ROOT,env=dict(os.environ,NO_COLOR='1',RUST_LOG='info'),creationflags=subprocess.CREATE_NEW_PROCESS_GROUP if os.name=='nt' else 0)
        java=probe=None
        try:
            tcp=wait(log,process,lambda text:re.search(r'Java listening.*?127\.0\.0\.1:(\d+)',text))
            nether=wait(log,process,lambda text:re.search(r'NetherNet listening.*?127\.0\.0\.1:(\d+)',text))
            java=wire.Client(('127.0.0.1',int(tcp[1])),'PickJavaProbe',inventory_fixture=False)
            def pick(position,data=False):wire.send(java.socket,0x24,wire.pos(*position)+bytes([data]))
            pick((1,3,1));picked(java,0,{0:(1,54)})
            for position in [(1,3,1),(1,2,1),(1,4,1),(63,3,63)]:
                pick(position);picked(java,0,{0:(1,54)})
            pick((1,3,1),True);picked(java,0,{0:(1,54)})
            # The native peer obtains its first item through block picking; only
            # later selection tests create materials through normal creative UI requests.
            probe=subprocess.Popen([str(native),'http://127.0.0.1:'+nether[1],'--block-pick'],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.STDOUT,text=True,encoding='utf-8')
            lines=[];events=queue.Queue()
            def receive():
                for line in probe.stdout:events.put(line)
            reader=threading.Thread(target=receive,daemon=True);reader.start()
            deadline=time.monotonic()+40
            while time.monotonic()<deadline:
                try:line=events.get(timeout=.1);lines.append(line)
                except queue.Empty:
                    assert probe.poll() is None,''.join(lines);continue
                if 'BLOCK_PICK_READY' in line:break
            else:raise TimeoutError(''.join(lines))
            wire.send(java.socket,0x29,wire.vi(0)+wire.pos(1,3,1)+b'\x01'+wire.vi(1));java.block((1,3,1),0,'air')
            probe.stdin.write('continue\n');probe.stdin.flush()
            code=probe.wait(timeout=15)
            reader.join(timeout=2)
            while not events.empty():lines.append(events.get())
            assert code==0 and 'PASS: native block pick' in ''.join(lines),''.join(lines)
            probe.stdin.close();probe=None
            # Java selects an already-owned main-inventory stack without changing its count.
            wire.send(java.socket,0x39,struct.pack('>h',20)+wire.vi(11)+wire.vi(55)+bytes(2))
            while True:
                id_,r=wire.receive_play(java)
                assert id_==0x6e
                slot=r.varint();stack=wire.item_stack(r);r.end()
                if slot==20:assert stack==(11,55);break
            pick((1,2,1));picked(java,0,{0:(11,55)})
            assert java.menu[20]==(1,54)
            # Existing hotbar reuse switches the selected slot and keeps the count.
            wire.send(java.socket,0x39,struct.pack('>h',40)+wire.vi(9)+wire.vi(54)+bytes(2))
            id_,r=wire.receive_play(java);assert id_==0x6e and r.varint()==4 and wire.item_stack(r)==(9,54);r.end()
            pick((2,3,1));picked(java,4,{0:(11,55),4:(9,54)})
            java.socket.close();java=None
            process.send_signal(signal.CTRL_BREAK_EVENT if os.name=='nt' else signal.SIGINT);assert process.wait(timeout=10)==0
            text=log.read_text(encoding='utf-8');assert 'plugin disabled' not in text and 'events inventory' in text and 'events equipment' in text
            assert 'protocol=777 id=36 ' in text and 'protocol=2225 id=34 ' in text
        except Exception:
            print(log.read_text(encoding='utf-8'));raise
        finally:
            if java is not None:java.socket.close()
            if probe is not None:
                if probe.poll() is None:probe.kill();probe.wait(timeout=5)
                probe.stdin.close()
            if process.poll() is None:process.kill();process.wait(timeout=10)
    print('PASS: Java/native block pick creation and reuse, selected-slot correction, counts and stack IDs, invalid-target rejection, shared held items and Rust inventory/equipment observations')

if __name__=='__main__':
    parser=argparse.ArgumentParser(description=__doc__);suffix='.exe' if os.name=='nt' else ''
    parser.add_argument('--server',type=Path,default=ROOT/('target/debug/sculith'+suffix))
    parser.add_argument('--client',type=Path,required=True)
    parser.add_argument('--observer',type=Path,default=ROOT/('target/debug/sculith-events-plugin'+suffix))
    args=parser.parse_args();smoke(args.server.resolve(strict=True),args.client.resolve(strict=True),args.observer.resolve(strict=True))
