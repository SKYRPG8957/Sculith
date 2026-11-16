#!/usr/bin/env python3
"""Real Rust game operations and mixed-edition requests under bounded profiling."""
import argparse
import json
import os
from pathlib import Path
import subprocess
import tempfile

import java_smoke as wire
from connection_smoke import status
from world_api_smoke import start, stop, wait, request

ROOT=Path(__file__).resolve().parents[2]


def check(path, reason):
    raw=path.read_text(encoding='utf-8');data=json.loads(raw)
    assert data['schema_version']==1 and data['scope']=='world_mailbox_wall_time'
    assert data['finished_reason']==reason
    assert 'MeterSecret' not in raw and 'world-result' not in raw and 'sculith_test_world' not in raw
    assert data['region_id']==0 and data['mode']=='profile'
    assert data['queue_capacity']==32 and 0<=data['occupied_capacity']<=32
    assert data['completed_commands']>=data['totals']['calls']>0
    assert data['aborted_commands']==data['totals']['aborted']==0
    assert 0<len(data['samples'])<=512 and 0<data['recent']['count']<=128
    assert data['owner_closed']==(reason=='shutdown')
    operations={op['operation']:op['totals'] for op in data['operations']}
    assert operations['reserve']['calls']>=2 and operations['activate']['calls']>=2
    assert operations['snapshot']['calls']>=2 and operations['plugin_world']['calls']>=2
    assert sum(op['calls'] for op in operations.values())==data['totals']['calls']
    for sample in data['samples']:
        times=sample['times']
        assert times['total_ns']==times['admission_ns']+times['queue_ns']+times['service_ns']
        assert not sample['aborted'] and sample['remaining_queue_depth']<=32
    assert data['recent']['p50']['queue_ns']<=data['recent']['p95']['queue_ns']<=data['recent']['p99']['queue_ns']
    if reason=='duration':assert not data['collecting']
    return data


def smoke(server, plugin, native):
    for duration,reason in [(5,'duration'),(300,'shutdown')]:
        with tempfile.TemporaryDirectory(prefix='sculith world profile ') as temporary:
            root=Path(temporary);output=root/'world.json';ipc=root/'plugins.json'
            args=['--profile-world',str(duration),'--world-profile-output',str(output),
                '--profile-plugins',str(duration),'--profile-output',str(ipc)]
            process,log,address,endpoint=start(server,plugin,root,extra_args=args)
            client=None
            try:
                status(address)
                client=wire.Client(address,'MeterSecret',inventory_fixture=False)
                peer=subprocess.run([str(native),endpoint,'--connections'],capture_output=True,text=True,encoding='utf-8',timeout=40)
                assert peer.returncode==0 and 'PASS: independent native post-spawn control reply' in peer.stdout,peer.stdout+peer.stderr
                initial=request(client,'read',{});assert initial['region']['revision']==0
                changed=request(client,'build',{(1,4,2):1,(17,4,2):10});assert changed['region']['revision']==1
                if reason=='duration':
                    wait(log,process,lambda text:'World mailbox profile saved' in text and 'Plugin IPC profile saved' in text,seconds=12)
                    check(output,reason);original=output.read_bytes()
                    # Expiry does not stop game work or later overwrite its report.
                    assert request(client,'read',{})['region']['revision']==1
                    status(address);client.socket.close();client=None;stop(process)
                    assert output.read_bytes()==original
                else:
                    client.socket.close();client=None;stop(process);check(output,reason)
                assert json.loads(ipc.read_text(encoding='utf-8'))['scope']=='plugin_ipc_wall_time'
                original=output.read_bytes()
                result=subprocess.run([str(server),'run','--config',str(root/'server.toml'),
                    '--profile-world','1','--world-profile-output',str(output)],capture_output=True,text=True,encoding='utf-8',timeout=5)
                assert result.returncode!=0 and 'already exists' in result.stderr
                assert output.read_bytes()==original
                collision=root/'collision.json'
                result=subprocess.run([str(server),'run','--config',str(root/'server.toml'),
                    '--profile-world','1','--world-profile-output',str(collision),
                    '--profile-plugins','1','--profile-output',str(collision)],capture_output=True,text=True,encoding='utf-8',timeout=5)
                assert result.returncode!=0 and 'separate output files' in result.stderr
                assert not collision.exists()
            except Exception:
                print(log.read_text(encoding='utf-8'));raise
            finally:
                try:
                    if client is not None:client.socket.close()
                finally:
                    if process.poll() is None:process.kill();process.wait(timeout=10)
    print('PASS: world admission/queue/service profiles, real Rust commits, Java/native joins, simultaneous plugin capture, private data omission, expiry/shutdown and no-clobber output')


if __name__=='__main__':
    parser=argparse.ArgumentParser(description=__doc__);suffix='.exe' if os.name=='nt' else ''
    parser.add_argument('--server',type=Path,default=ROOT/('target/debug/sculith'+suffix))
    parser.add_argument('--plugin',type=Path,default=ROOT/('target/debug/sculith-world-fixture'+suffix))
    parser.add_argument('--client',type=Path,required=True)
    args=parser.parse_args();smoke(args.server.resolve(strict=True),args.plugin.resolve(strict=True),args.client.resolve(strict=True))
