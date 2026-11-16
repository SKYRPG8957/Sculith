#!/usr/bin/env python3
"""Real Rust UI plugin with independent Java NBT and Bedrock form responses."""
import argparse
import os
from pathlib import Path
import re
import json
import signal
import subprocess
import tempfile
import time

import java_smoke as wire
from plugin_game_smoke import nbt_text

ROOT = Path(__file__).resolve().parents[2]


def nbt_string(reader):
    raw = reader.take(reader.number(">H"))
    assert not any(byte >= 0xf0 for byte in raw), "four-byte UTF-8 in Java NBT"
    return raw.replace(b"\xc0\x80", b"\0").decode("utf-8", "surrogatepass").encode("utf-16", "surrogatepass").decode("utf-16")


def nbt(reader, tag=None, depth=0):
    assert depth <= 16
    if tag is None: tag = reader.number(">B")
    if tag == 1: return reader.number(">b")
    if tag == 3: return reader.number(">i")
    if tag == 5: return reader.number(">f")
    if tag == 8: return nbt_string(reader)
    if tag == 9:
        kind, count = reader.number(">B"), reader.number(">i")
        assert 0 <= count <= 32
        return [nbt(reader, kind, depth+1) for _ in range(count)]
    if tag == 10:
        result = {}
        for _ in range(32):
            kind = reader.number(">B")
            if kind == 0: return result
            key = nbt_string(reader)
            assert key not in result
            result[key] = nbt(reader, kind, depth+1)
        raise AssertionError("excess compound fields")
    raise AssertionError(("unexpected NBT type", tag))


def dialog(client):
    id_, reader = wire.receive_play(client)
    assert id_ == 143 and reader.varint() == 0
    value = nbt(reader); reader.end()
    assert value["type"] == "minecraft:multi_action"
    assert value["title"] == "Sculith에 오신 것을 환영합니다 👋"
    assert value["pause"] == 0 and value["can_close_with_escape"] == 1
    assert value["body"][0]["contents"] == "Java와 베드락에서 같은 메뉴를 사용합니다.\n원하는 항목을 선택하세요."
    assert [item["label"] for item in value["actions"]] == ["기능 안내", "닫기"]
    first = value["actions"][0]["action"]["id"]
    match = re.fullmatch(r"sculith:form/(\d+)/0", first)
    assert match
    form_id = int(match[1]); assert form_id != 0
    assert value["exit_action"]["action"]["id"] == f"sculith:form/{form_id}/cancel"
    return form_id


def click(client, form_id, selection):
    wire.send(client.socket, 68, wire.string(f"sculith:form/{form_id}/{selection}") + b"\x01\x00")


def message(client, cancelled=False):
    id_, reader = wire.receive_play(client)
    assert id_ == 124
    expected = "Sculith: 메뉴를 닫았습니다." if cancelled else "Sculith: 블록을 설치하거나 부수고, Ctrl+C로 서버를 종료하면 월드를 저장합니다."
    assert nbt_text(reader) == expected and reader.number(">B") == 0
    reader.end()


def smoke(server, plugin, native_client, api_version=11):
    with tempfile.TemporaryDirectory(prefix="sculith forms ") as temporary:
        root = Path(temporary); settings=root/"server.toml";log=root/"server.log"
        settings.write_text('[java]\nsessions=true\nrequire_authentication=false\n'
                            '[bedrock]\nsessions=true\nrequire_authentication=false\n'
                            'nethernet_identity_path='+json.dumps(str(root/"identity.pem"))+'\n'
                            '[network.java]\nbind="127.0.0.1:0"\n[network.bedrock]\nbind="127.0.0.1:0"\n'
                            '[[plugins]]\npath='+json.dumps(str(plugin))+'\n'
                            '[[plugins]]\npath='+json.dumps(str(plugin))+'\nargs=["--observe"]\n',encoding="utf-8")
        with log.open("w",encoding="utf-8") as output:
            process=subprocess.Popen([str(server),"run","--config",str(settings)],stdout=output,stderr=output,cwd=ROOT,
                                      env=dict(os.environ,NO_COLOR="1",RUST_LOG="info"),
                                      creationflags=subprocess.CREATE_NEW_PROCESS_GROUP if os.name=="nt" else 0)
            java=None;cancelled=None
            try:
                deadline=time.monotonic()+5
                while time.monotonic()<deadline:
                    text=log.read_text(encoding="utf-8")
                    tcp=re.search(r'Java listening.*?127\.0\.0\.1:(\d+)',text)
                    native=re.search(r'NetherNet listening.*?127\.0\.0\.1:(\d+)',text)
                    if tcp and native: break
                    assert process.poll() is None,text
                    time.sleep(.05)
                else: raise TimeoutError(text)
                assert "forms-demo" in text and "forms-observer" in text and f"api_version={api_version}" in text
                address=("127.0.0.1",int(tcp[1]))
                java=wire.Client(address,"FormJavaProbe")
                form_id=dialog(java)
                click(java,form_id,99)  # Must retain the valid pending form.
                for mode in ("--form","--form-cancel"):
                    result=subprocess.run([str(native_client),"http://127.0.0.1:"+native[1],mode],capture_output=True,text=True,encoding="utf-8",timeout=40)
                    assert result.returncode==0,result.stdout+result.stderr
                    assert "PASS: independent Bedrock shared form and Rust callback" in result.stdout
                # No native form or private result may be queued for Java. Its
                # original form must survive the other session's forged reply.
                click(java,form_id,0);message(java)
                click(java,form_id,0)  # Replay cannot create another callback.
                cancelled=wire.Client(address,"FormCancelProbe")
                cancel_id=dialog(cancelled);click(cancelled,cancel_id,"cancel");message(cancelled,True)
                java.socket.close();java=None
                cancelled.socket.close();cancelled=None
                process.send_signal(signal.CTRL_BREAK_EVENT if os.name=="nt" else signal.SIGINT)
                assert process.wait(timeout=5)==0
                text=log.read_text(encoding="utf-8")
                assert text.count("forms reply request=7")==4,text
                assert "unexpected foreign form reply" not in text and "plugin disabled" not in text and "plugin form rejected" not in text
                assert "forms plugin stopped" in text
            except Exception:
                print(log.read_text(encoding="utf-8"));raise
            finally:
                for client in (java,cancelled):
                    if client is not None: client.socket.close()
                if process.poll() is None: process.kill();process.wait(timeout=5)
    print("PASS: real Rust API-v3 shared forms, independent Unicode Java dialog and Bedrock form, choices/cancellation, replay/session/recipient/plugin-owner isolation and graceful stopping")


if __name__=="__main__":
    parser=argparse.ArgumentParser(description=__doc__);suffix=".exe" if os.name=="nt" else ""
    parser.add_argument("--server",type=Path,default=ROOT/("target/debug/sculith"+suffix))
    parser.add_argument("--plugin",type=Path,default=ROOT/("target/debug/sculith-forms-plugin"+suffix))
    parser.add_argument("--client",type=Path,required=True)
    parser.add_argument("--api-version",type=int,choices=(3,4,5,6,7,8,9,10,11),default=11)
    args=parser.parse_args()
    smoke(args.server.resolve(strict=True),args.plugin.resolve(strict=True),args.client.resolve(strict=True),args.api_version)
