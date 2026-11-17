#!/usr/bin/env python3
"""Real Rust command sources, legacy isolation, world edits and worker failure."""
import argparse
import json
import os
from pathlib import Path
import re
import socket
import subprocess
import tempfile
import time

import java_smoke as wire
from console_smoke import wait
from plugin_game_smoke import nbt_text

ROOT=Path(__file__).resolve().parents[2]

def smoke(server,plugin,native):
    with tempfile.TemporaryDirectory(prefix="sculith console commands ") as temporary:
        root=Path(temporary);config=root/'server.toml';log=root/'server.log'
        text='[java]\nsessions=true\nrequire_authentication=false\n[bedrock]\nsessions=true\nrequire_authentication=false\nnethernet_identity_path='+json.dumps(str(root/'identity.pem'))+'\n[network.java]\nbind="127.0.0.1:0"\n[network.bedrock]\nbind="127.0.0.1:0"\n'
        for mode in ['console','observer','dual','legacy','slow']:
            text+='[[plugins]]\npath='+json.dumps(str(plugin))+'\nargs='+json.dumps([mode])+'\ntimeout_ms=1000\n'
        config.write_text(text,encoding='utf-8')
        with log.open('w',encoding='utf-8') as output:
            process=subprocess.Popen([str(server),'run','--config',str(config),'--profile-plugins','30','--profile-output',str(root/'profile.json')],stdin=subprocess.PIPE,stdout=output,stderr=output,cwd=ROOT,env=dict(os.environ,NO_COLOR='1',RUST_LOG='info'),creationflags=subprocess.CREATE_NEW_PROCESS_GROUP if os.name=='nt' else 0)
        java=None
        def command(value):process.stdin.write(value.encode('utf-8')+b'\n');process.stdin.flush()
        def has(value):return wait(process,log,lambda text:value in text)
        def ack():
            id_,reader=wire.receive_play(java);assert id_==124
            assert nbt_text(reader)=='command acknowledged' and reader.number('>B')==0;reader.end()
        try:
            has('Server started')
            text=log.read_text(encoding='utf-8');assert 'plugin=console-legacy version=1 api_version=11' in text
            assert text.count('api_version=12')==4
            java_port=int(re.search(r'Java listening.*?127\.0\.0\.1:(\d+)',text)[1]);native_port=re.search(r'NetherNet listening.*?127\.0\.0\.1:(\d+)',text)[1]
            value='한글 ; $(echo never)'
            command('console_probe '+value);has('console-reply console '+json.dumps(value,ensure_ascii=False))
            command('dual_probe terminal');has('console-reply dual "terminal"')
            command('legacy_probe private');has('This command is not available from this source.')
            java=wire.Client(('127.0.0.1',java_port),'ConsoleCmdJava',inventory_fixture=False)
            assert set(java.commands)=={'help','legacy_probe','dual_probe','observer_probe'}
            wire.send(java.socket,7,wire.string('legacy_probe old-client'));ack();has('player-reply legacy "old-client"')
            time.sleep(.52);wire.send(java.socket,7,wire.string('dual_probe java'));ack();has('player-reply dual "java"')
            result=subprocess.run([str(native),'http://127.0.0.1:'+native_port,'--console-commands'],capture_output=True,text=True,encoding='utf-8',timeout=45)
            assert result.returncode==0,result.stdout+result.stderr
            assert 'PASS: native command catalogs' in result.stdout
            has('player-reply dual "native"')
            java.socket.settimeout(.2)
            try:
                frame=wire.receive_play(java);raise AssertionError(('foreign command reply leaked',frame[0]))
            except socket.timeout:pass
            java.socket.settimeout(5)
            command('console_probe edit');has('console-world ')
            id_,reader=wire.receive_play(java);assert id_==8
            assert reader.take(8)==wire.pos(1,4,1) and reader.varint()==1;reader.end()
            id_,reader=wire.receive_play(java);assert id_==0x31 and [reader.varint(),reader.varint()]==[0,0]
            heights=[4]*256;heights[17]=5;wire.light(reader,heights=heights);reader.end()
            command('slow_probe private');has('Command failed; the plugin was disabled.')
            command('slow_probe again');has('This command is temporarily unavailable.')
            command('console_probe alive');has('console-reply console "alive"')
            command('stop');assert process.wait(timeout=10)==0
            text=log.read_text(encoding='utf-8')
            assert text.count('plugin disabled')==1 and 'plugin=console-slow' in text
            assert 'foreign' not in text and 'stolen' not in text
            assert 'console-reply observer' not in text and 'console-reply legacy' not in text
            report=(root/'profile.json').read_text(encoding='utf-8');assert value not in report and 'stolen' not in report
            assert '"console_command"' in report
        except Exception:
            print(log.read_text(encoding='utf-8'));raise
        finally:
            if java is not None:java.socket.close()
            process.stdin.close()
            if process.poll() is None:process.kill();process.wait(timeout=10)
    print('PASS: private Rust API-v12 console commands without players, v11 player commands, Java/native catalogs and source validation, shared world edits and timeout isolation')

if __name__=='__main__':
    parser=argparse.ArgumentParser(description=__doc__);suffix='.exe' if os.name=='nt' else ''
    parser.add_argument('--server',type=Path,default=ROOT/('target/debug/sculith'+suffix))
    parser.add_argument('--plugin',type=Path,default=ROOT/('target/debug/sculith-console-fixture'+suffix))
    parser.add_argument('--client',type=Path,required=True)
    args=parser.parse_args();smoke(args.server.resolve(strict=True),args.plugin.resolve(strict=True),args.client.resolve(strict=True))
