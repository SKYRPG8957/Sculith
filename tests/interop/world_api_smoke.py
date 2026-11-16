#!/usr/bin/env python3
"""Real Rust world API, atomic commits, private replies and shared Java/native state."""
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
from connection_smoke import status
from plugin_game_smoke import nbt_text

ROOT=Path(__file__).resolve().parents[2]

def wait(log, process, predicate, seconds=10):
    deadline=time.monotonic()+seconds
    while time.monotonic()<deadline:
        text=log.read_text(encoding="utf-8")
        value=predicate(text)
        if value:return value
        assert process.poll() is None,text
        time.sleep(.02)
    raise TimeoutError(log.read_text(encoding="utf-8"))

def start(server, plugin, root, flood=False, autosave_interval=60):
    settings,log=root/'server.toml',root/'server.log'
    config='[java]\nsessions=true\nrequire_authentication=false\n[bedrock]\nsessions=true\nrequire_authentication=false\nnethernet_identity_path='+json.dumps(str(root/'identity.pem'))+'\n[network.java]\nbind="127.0.0.1:0"\n[network.bedrock]\nbind="127.0.0.1:0"\n'
    config+='[world]\nsnapshot_path='+json.dumps(str(root/'region.sculith'))+'\nautosave_interval_secs='+str(autosave_interval)+'\n'
    modes=['--flood','--guard'] if flood else ['', '--observe','--guard','--legacy']
    for mode in modes:
        config+='[[plugins]]\npath='+json.dumps(str(plugin))+'\nargs='+json.dumps([mode] if mode else [])+'\ntimeout_ms=1000\n'
    settings.write_text(config,encoding='utf-8')
    with log.open('w',encoding='utf-8') as output:
        process=subprocess.Popen([str(server),'run','--config',str(settings)],cwd=ROOT,stdout=output,stderr=output,
            env=dict(os.environ,NO_COLOR='1',RUST_LOG='info'),creationflags=subprocess.CREATE_NEW_PROCESS_GROUP if os.name=='nt' else 0)
    tcp=wait(log,process,lambda text:re.search(r'Java listening.*?127\.0\.0\.1:(\d+)',text))
    udp=wait(log,process,lambda text:re.search(r'NetherNet listening.*?127\.0\.0\.1:(\d+)',text))
    return process,log,('127.0.0.1',int(tcp[1])),'http://127.0.0.1:'+udp[1]

def stop(process):
    process.send_signal(signal.CTRL_BREAK_EVENT if os.name=='nt' else signal.SIGINT)
    assert process.wait(timeout=10)==0

def position(reader):
    word=reader.number('>Q')
    def signed(value,bits):return value-(1<<bits) if value&(1<<(bits-1)) else value
    return signed(word>>38,26),signed(word&4095,12),signed((word>>12)&0x3ffffff,26)

def request(client,command,expected_updates=None):
    # Respect the owner's existing two-command/second input limit.
    time.sleep(.52)
    wire.send(client.socket,7,wire.string('sculith_test_world '+command))
    updates={}
    for _ in range(128):
        id_,reader=wire.receive_play(client)
        if id_==8:
            point=position(reader);state=reader.varint();reader.end()
            assert point not in updates,('duplicate delta',point)
            updates[point]=state
        elif id_==124:
            message=nbt_text(reader);assert reader.number('>B')==0;reader.end()
            assert message.startswith('world-result '),message
            if expected_updates is not None:assert updates==expected_updates,(updates,expected_updates)
            return json.loads(message.removeprefix('world-result '))
        else:
            assert id_==0x31,('unexpected world frame',id_)
    raise AssertionError('private world response missing')

def smoke(server,plugin,native):
    with tempfile.TemporaryDirectory(prefix='sculith world api ') as temporary:
        process,log,address,endpoint=start(server,plugin,Path(temporary))
        client=probe=None;lines=[]
        try:
            wait(log,process,lambda text:'world discovery completed before player requests' in text)
            client=wire.Client(address,'WorldJavaProbe')
            probe=subprocess.Popen([str(native),endpoint,'--world-api'],stdout=subprocess.PIPE,stderr=subprocess.STDOUT,text=True,encoding='utf-8')
            events=queue.Queue()
            def output():
                for line in probe.stdout:lines.append(line);events.put(line.strip())
                events.put(None)
            threading.Thread(target=output,daemon=True).start()
            def native_stage(label):
                deadline=time.monotonic()+25
                while time.monotonic()<deadline:
                    value=events.get(timeout=max(.01,deadline-time.monotonic()))
                    assert value is not None,''.join(lines)
                    if value==label:return
                raise TimeoutError(''.join(lines))
            native_stage('WORLD_NATIVE_READY')
            initial=request(client,'read',{})
            assert initial['type']=='snapshot' and initial['region']['revision']==0
            assert [b['block'] for b in initial['blocks']]==['minecraft:air']*3
            built=request(client,'build',{(1,4,2):1,(17,4,2):10})
            assert built['type']=='updated' and built['region']['revision']==1 and len(built['changes'])==2
            native_stage('WORLD_NATIVE_BUILT')
            for command,reason in [('stale','revision_conflict'),('bounds','outside_region'),('duplicate','invalid_request'),('unsupported','unsupported_block')]:
                rejected=request(client,command,{})
                assert rejected['type']=='rejected' and rejected['reason']['type']==reason,rejected
            unchanged=request(client,'read',{})
            assert unchanged['region']['revision']==1 and [b['block'] for b in unchanged['blocks']]==['minecraft:stone','minecraft:dirt','minecraft:air']
            noop=request(client,'noop',{})
            assert noop['type']=='updated' and noop['changes']==[] and noop['region']['revision']==1
            erased=request(client,'erase',{(1,4,2):0,(17,4,2):0})
            assert erased['region']['revision']==2 and len(erased['changes'])==2
            native_stage('WORLD_NATIVE_ERASED')
            wire.send(client.socket,0x42,wire.vi(0)+wire.pos(2,3,2)+wire.vi(1)+struct.pack('>fffBB',.5,1,.5,0,0)+wire.vi(1))
            changed=False
            for _ in range(32):
                id_,reader=wire.receive_play(client)
                if id_==8:
                    assert position(reader)==(2,4,2) and reader.varint()==1;reader.end();changed=True;break
                assert id_ in (4,0x31),id_
            assert changed
            native_stage('WORLD_NATIVE_PLAYER_COMMIT')
            assert probe.wait(timeout=5)==0,''.join(lines)
            final=request(client,'read',{})
            assert final['region']['revision']==3 and [b['block'] for b in final['blocks']]==['minecraft:air','minecraft:air','minecraft:stone']
            persisted=request(client,'build',{(1,4,2):1,(17,4,2):10})
            assert persisted['region']['revision']==4
            client.socket.close();client=None
            stop(process)
            text=log.read_text(encoding='utf-8')
            assert 'api_version=10' in text and 'plugin disabled' not in text,text
            audits=[json.loads('{'+line.split('world-audit {',1)[1]) for line in text.splitlines() if 'world-audit {' in line]
            assert [a['revision'] for a in audits]==[1,2,3,4],audits
            assert [a['actor'] for a in audits]==[{'type':'plugin','name':'world-fixture'},{'type':'plugin','name':'world-fixture'},{'type':'player','player_id':1},{'type':'plugin','name':'world-fixture'}],audits
            assert 'legacy-world-change player=1 revision=3 count=1' in text
            assert text.count('legacy-world-change')==1
            assert 'world fixture stopped mode=observe notifications=4' in text
            assert 'world fixture stopped mode=guard notifications=0' in text
            assert 'foreign world response' not in text
            # A new process restores the plugin-edited region and its CAS revision.
            process,log,address,_=start(server,plugin,Path(temporary))
            wait(log,process,lambda text:'world discovery completed before player requests' in text)
            client=wire.Client(address,'WorldReload',edits={(1,4,2):1,(17,4,2):10,(2,4,2):1})
            restored=request(client,'read',{})
            assert restored['region']['revision']==4
            assert [b['block'] for b in restored['blocks']]==['minecraft:stone','minecraft:dirt','minecraft:stone']
            client.socket.close();client=None
            stop(process)
        except Exception:
            print(log.read_text(encoding='utf-8'));print(''.join(lines));raise
        finally:
            if client:client.socket.close()
            if probe and probe.poll() is None:probe.kill();probe.wait(timeout=5)
            if process.poll() is None:process.kill();process.wait(timeout=5)
    with tempfile.TemporaryDirectory(prefix='sculith world budget ') as temporary:
        process,log,address,_=start(server,plugin,Path(temporary),True)
        try:
            wait(log,process,lambda text:'plugin world request limit exceeded' in text)
            status(address)
            stop(process)
            assert 'foreign world response' not in log.read_text(encoding='utf-8')
        except Exception:
            print(log.read_text(encoding='utf-8'));raise
        finally:
            if process.poll() is None:process.kill();process.wait(timeout=5)
    print('PASS: Rust API-v10 private world reads, atomic plugin edits, CAS/bounds/type/duplicate/no-op rejection, shared Java/native deltas, player/plugin attribution, legacy hooks, persisted plugin edits and quota isolation')

if __name__=='__main__':
    parser=argparse.ArgumentParser(description=__doc__);suffix='.exe' if os.name=='nt' else ''
    parser.add_argument('--server',type=Path,default=ROOT/('target/debug/sculith'+suffix))
    parser.add_argument('--plugin',type=Path,default=ROOT/('target/debug/sculith-world-fixture'+suffix))
    parser.add_argument('--client',type=Path,required=True)
    args=parser.parse_args();smoke(args.server.resolve(strict=True),args.plugin.resolve(strict=True),args.client.resolve(strict=True))
