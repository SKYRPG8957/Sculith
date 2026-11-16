#!/usr/bin/env python3
"""Bounded IPC timing reports from real Rust workers; not CPU sampling."""
import argparse
import json
import os
from pathlib import Path
import re
import signal
import subprocess
import tempfile

from connection_smoke import status, wait_for

ROOT = Path(__file__).resolve().parents[2]


def start(server, fixture, root, duration):
    config, log, output = root/'server.toml', root/'server.log', root/'profile.json'
    config.write_text('[server]\nmotd="profile-private-payload-sentinel"\n'
        '[network.java]\nbind="127.0.0.1:0"\n'
        '[network.bedrock]\nenabled=false\nbind="127.0.0.1:0"\n'
        '[[plugins]]\npath='+json.dumps(str(fixture))+'\nargs=["active","tick"]\n'
        '[[plugins]]\npath='+json.dumps(str(fixture))+'\nargs=["peer","tick"]\n'
        '[[plugins]]\npath='+json.dumps(str(fixture))+'\nargs=["idle","none"]\n', encoding='utf-8')
    with log.open('w', encoding='utf-8') as stream:
        process = subprocess.Popen([str(server), 'run', '--config', str(config),
            '--profile-plugins', str(duration), '--profile-output', str(output)], cwd=ROOT,
            stdout=stream, stderr=stream, env=dict(os.environ, NO_COLOR='1', RUST_LOG='info'),
            creationflags=subprocess.CREATE_NEW_PROCESS_GROUP if os.name == 'nt' else 0)
    match = wait_for(log, process, lambda text: re.search(r'Java listening.*?127\.0\.0\.1:(\d+)', text))
    return process, log, config, output, ('127.0.0.1', int(match[1]))


def stop(process):
    process.send_signal(signal.CTRL_BREAK_EVENT if os.name == 'nt' else signal.SIGINT)
    assert process.wait(timeout=10) == 0


def check(output, reason):
    data = json.loads(output.read_text(encoding='utf-8'))
    assert data['schema_version'] == 1 and data['scope'] == 'plugin_ipc_wall_time'
    assert data['finished_reason'] == reason
    assert 'profile-private-payload-sentinel' not in output.read_text(encoding='utf-8')
    plugins = {plugin['name']: plugin for plugin in data['plugins']}
    assert set(plugins) == {'active', 'peer', 'idle'} and not any(plugin['disabled'] for plugin in plugins.values())
    active, peer, idle = (plugins[name]['ipc'] for name in ['active', 'peer', 'idle'])
    assert active['calls'] == peer['calls']
    # Both tick workers share one encoded body; only Hello is serialized locally.
    assert active['message_encodes'] == peer['message_encodes'] == idle['message_encodes'] == 1
    assert data['dispatcher']['message_encodes'] > 0
    assert all(sample['message_encodes'] == 1 for sample in data['dispatcher']['samples'] if sample['hook'] == 'tick')
    assert all(sample['message_encodes'] == 0 for sample in active['samples'])
    assert 0 < data['shared_event_buffer_capacity'] <= 65536
    assert all(0 < plugin[key] <= 65536 for plugin in plugins.values()
        for key in ['request_buffer_capacity', 'reply_buffer_capacity'])
    assert active['calls'] > 1 and active['timed_calls'] > 0 and active['failures'] == active['timeouts'] == 0
    assert active['sent_bytes'] > 0 and active['received_bytes'] > 0 and active['encode_ns'] > 0
    assert 0 < active['recent_p50_ns'] <= active['recent_p95_ns'] <= active['recent_p99_ns']
    assert 0 < len(active['samples']) <= 512
    assert [sample['request_id'] for sample in active['samples']] == sorted({sample['request_id'] for sample in active['samples']})
    if reason == 'duration':
        assert idle['timed_calls'] == 0 and idle['samples'] == []
    else:
        assert idle['timed_calls'] <= 1
    assert all('hook' in sample and not sample['failed'] for sample in active['samples'])
    if reason == 'duration':
        assert not active['collecting'] and idle['calls'] == 1
    return data


def smoke(server, fixture):
    for duration, reason in [(1, 'duration'), (300, 'shutdown')]:
        with tempfile.TemporaryDirectory(prefix='sculith plugin profile ') as temporary:
            root = Path(temporary)
            process, log, config, output, address = start(server, fixture, root, duration)
            try:
                status(address)
                if reason == 'duration':
                    wait_for(log, process, lambda text: 'Plugin IPC profile saved' in text)
                    check(output, reason)
                    original = output.read_bytes()
                    status(address)
                    stop(process)
                    assert output.read_bytes() == original
                else:
                    stop(process)
                    check(output, reason)
                original = output.read_bytes()
                result = subprocess.run([str(server), 'run', '--config', str(config), '--profile-plugins', '1',
                    '--profile-output', str(output)], capture_output=True, text=True, encoding='utf-8', timeout=5)
                assert result.returncode != 0 and 'already exists' in result.stderr
                assert output.read_bytes() == original
            except Exception:
                print(log.read_text(encoding='utf-8')); raise
            finally:
                if process.poll() is None: process.kill(); process.wait(timeout=10)
    print('PASS: shared event encoding, bounded reusable frames, real-Rust IPC timing reports, exact recent quantiles, private payload omission, automatic expiry, interrupted capture and no-clobber output')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__); suffix = '.exe' if os.name == 'nt' else ''
    parser.add_argument('--server', type=Path, default=ROOT/('target/debug/sculith'+suffix))
    parser.add_argument('--plugin', type=Path, default=ROOT/('target/debug/sculith-profile-fixture'+suffix))
    args = parser.parse_args()
    smoke(args.server.resolve(strict=True), args.plugin.resolve(strict=True))
