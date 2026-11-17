#!/usr/bin/env python3
"""Rust typed input callbacks shared by native forms and vanilla Java dialogs."""
import argparse
import json
import os
from pathlib import Path
import re
import signal
import struct
import subprocess
import tempfile

import java_smoke as wire
from forms_smoke import nbt
from plugin_game_smoke import nbt_text
from connection_smoke import wait_for, status

ROOT = Path(__file__).resolve().parents[2]


def start(server, plugin, root):
    config, log = root/'server.toml', root/'server.log'
    config.write_text('[java]\nsessions=true\nrequire_authentication=false\n'
        '[bedrock]\nsessions=true\nrequire_authentication=false\nnethernet_identity_path='+json.dumps(str(root/'identity.pem'))+'\n'
        '[network.java]\nbind="127.0.0.1:0"\n[network.bedrock]\nbind="127.0.0.1:0"\n'
        '[[plugins]]\npath='+json.dumps(str(plugin))+'\nargs=["--inputs"]\n'
        '[[plugins]]\npath='+json.dumps(str(plugin))+'\nargs=["--inputs","--observe"]\n', encoding='utf-8')
    with log.open('w', encoding='utf-8') as output:
        process = subprocess.Popen([str(server), 'run', '--config', str(config)], cwd=ROOT,
            stdout=output, stderr=output, env=dict(os.environ, NO_COLOR='1', RUST_LOG='info'),
            creationflags=subprocess.CREATE_NEW_PROCESS_GROUP if os.name == 'nt' else 0)
    tcp = wait_for(log, process, lambda text: re.search(r'Java listening.*?127\.0\.0\.1:(\d+)', text))
    udp = wait_for(log, process, lambda text: re.search(r'NetherNet listening.*?127\.0\.0\.1:(\d+)', text))
    return process, log, ('127.0.0.1', int(tcp[1])), 'http://127.0.0.1:'+udp[1]


def dialog(client):
    id_, reader = wire.receive_play(client)
    assert id_ == 143 and reader.varint() == 0
    value = nbt(reader); reader.end()
    assert value['type'] == 'minecraft:multi_action' and value['title'] == '공통 입력 폼 👋'
    assert value['pause'] == 0 and value['can_close_with_escape'] == 1
    inputs = value['inputs']
    assert [field['key'] for field in inputs] == ['text_0', 'toggle_1', 'number_2', 'number_3']
    assert [field['label'] for field in inputs] == ['이름', '알림 받기', '표시 방식', '크기']
    assert [field['type'] for field in inputs] == ['minecraft:text', 'minecraft:boolean', 'minecraft:single_option', 'minecraft:number_range']
    assert inputs[0]['initial'] == '테스트 👋' and inputs[0]['max_length'] == 16
    assert inputs[1]['initial'] == 1
    assert inputs[2]['options'] == [{'id':'0', 'display':'기본', 'initial':1}, {'id':'1', 'display':'확장', 'initial':0}]
    assert [inputs[3][name] for name in ['start', 'end', 'step', 'initial']] == [-10., 10., 5., 0.]
    assert len(value['actions']) == 1 and value['actions'][0]['label'] == '확인'
    action = value['actions'][0]['action']
    assert action['type'] == 'minecraft:dynamic/custom'
    match = re.fullmatch(r'sculith:input/(\d+)/submit', action['id']); assert match
    form_id = int(match[1]); assert form_id > 0
    assert value['exit_action']['action']['id'] == f'sculith:input/{form_id}/cancel'
    return form_id


def mutf8(value):
    raw = value.encode('utf-16-be', 'surrogatepass')
    units = struct.unpack('>'+str(len(raw)//2)+'H', raw)
    raw = ''.join(chr(unit) for unit in units).encode('utf-8', 'surrogatepass').replace(b'\0', b'\xc0\x80')
    return struct.pack('>H', len(raw))+raw


def click(client, form_id, entries=None, payload=None):
    action = 'cancel' if entries is None and payload is None else 'submit'
    if payload is None:
        if entries is None:
            payload = b'\0'
        else:
            payload = bytearray(b'\x0a')
            for key, value in entries:
                tag = 1 if key.startswith('toggle_') and value in ('true','false') else 5 if key == 'number_3' else 8
                payload += bytes([tag])+mutf8(key)
                if tag == 1: payload += bytes([int(value == 'true')])
                elif tag == 5: payload += struct.pack('>f',float(value))
                else: payload += mutf8(value)
            payload += b'\0'
    wire.send(client.socket, 68, wire.string(f'sculith:input/{form_id}/{action}')+wire.vi(len(payload))+payload)


def message(client, cancelled=False):
    id_, reader = wire.receive_play(client)
    assert id_ == 124
    expected = '입력을 취소했습니다.' if cancelled else '입력: 이름 👋 | 알림: false | 선택: 1 | 값: 5'
    assert nbt_text(reader) == expected and reader.number('>B') == 0
    reader.end()


def smoke(server, plugin, native):
    with tempfile.TemporaryDirectory(prefix='sculith input forms ') as temporary:
        process, log, address, endpoint = start(server, plugin, Path(temporary))
        java = cancel = None
        try:
            java = wire.Client(address, 'InputJava', inventory_fixture=False)
            form_id = dialog(java)
            valid = [('number_3', '5.0'), ('toggle_1', 'false'), ('text_0', '이름 👋'), ('number_2', '1')]
            for entries in [valid[:-1], [('text_0', 'x'), ('text_0', 'duplicate')],
                            [('toggle_0', 'true')], [('number_0', 'NaN')], [('number_3', 'NaN')],
                            [('text_0', '👋'*9), *valid[:2], valid[3]],
                            [('text_0', 'x'), ('toggle_1', 'true'), ('number_2', '9'), ('number_3', '3')]]:
                click(java, form_id, entries)
            click(java, form_id, payload=b'\x0a\x09\x00\x01x\x00\x00\x00\x00\x00\x00')
            for mode in ['--inputs', '--inputs-cancel']:
                result = subprocess.run([str(native), endpoint, mode], capture_output=True, text=True, encoding='utf-8', timeout=40)
                assert result.returncode == 0, result.stdout+result.stderr
                assert 'PASS: native typed input form and private Rust callback' in result.stdout
            # The other session's forged ID and invalid arrays must not consume
            # Java's request or expose its text to this player or another plugin.
            click(java, form_id, valid); message(java)
            click(java, form_id, valid)
            cancel = wire.Client(address, 'InputCancel', inventory_fixture=False)
            cancelled_id = dialog(cancel)
            click(cancel, cancelled_id); message(cancel, True)
            click(cancel, cancelled_id)
            status(address)
            java.socket.close(); java = None
            cancel.socket.close(); cancel = None
            process.send_signal(signal.CTRL_BREAK_EVENT if os.name == 'nt' else signal.SIGINT)
            assert process.wait(timeout=10) == 0
            text = log.read_text(encoding='utf-8')
            assert 'api_version=12' in text and text.count('input reply request=19') == 4, text
            assert 'unexpected foreign input reply' not in text and 'plugin disabled' not in text, text
            assert text.count('cancelled=true') == 2 and text.count('cancelled=false') == 2, text
        except Exception:
            print(log.read_text(encoding='utf-8')); raise
        finally:
            for client in [java, cancel]:
                if client: client.socket.close()
            if process.poll() is None: process.kill(); process.wait(timeout=10)
    print('PASS: Rust API-v11 text/toggle/dropdown/integer-slider UI, Java/native typed replies and cancellation, UTF-16 limits, malformed/replay/session/recipient isolation')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__); suffix = '.exe' if os.name == 'nt' else ''
    parser.add_argument('--server', type=Path, default=ROOT/('target/debug/sculith'+suffix))
    parser.add_argument('--plugin', type=Path, default=ROOT/('target/debug/sculith-forms-plugin'+suffix))
    parser.add_argument('--client', type=Path, required=True)
    args = parser.parse_args()
    smoke(args.server.resolve(strict=True), args.plugin.resolve(strict=True), args.client.resolve(strict=True))
