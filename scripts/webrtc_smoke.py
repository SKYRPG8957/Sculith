#!/usr/bin/env python3
"""Probe signed NetherNet + Bedrock settings with independent aiortc/OpenSSL."""
import argparse
import asyncio
import base64
import json
import os
from pathlib import Path
import re
import signal
import struct
import subprocess
import tempfile
import time
import urllib.request
import uuid
import zlib

from aiortc import RTCConfiguration, RTCPeerConnection, RTCSessionDescription
from cryptography.hazmat.primitives import hashes, serialization
from cryptography.hazmat.primitives.asymmetric import ec
from cryptography.hazmat.primitives.asymmetric.utils import decode_dss_signature, encode_dss_signature

ROOT = Path(__file__).resolve().parents[1]


def url64(data):
    return base64.urlsafe_b64encode(data).rstrip(b"=").decode()


def un64(data):
    return base64.urlsafe_b64decode(data + "=" * (-len(data) % 4))


def compact(data):
    return json.dumps(data, separators=(",", ":"), sort_keys=True).encode()


def fingerprint_payload(sdp):
    return compact({"fingerprint": [
        {"algorithm": algorithm, "digest": digest}
        for algorithm, digest in re.findall(r"^a=fingerprint:([^\s]+) ([^\r\n]+)", sdp, re.M)
    ]})


def sign_offer(sdp, key):
    header = url64(b'{"alg":"ES384"}')

    def sign(data):
        r, s = decode_dss_signature(key.sign(data.encode(), ec.ECDSA(hashes.SHA384())))
        return url64(r.to_bytes(48, "big") + s.to_bytes(48, "big"))

    cpk = base64.b64encode(key.public_key().public_bytes(
        serialization.Encoding.DER, serialization.PublicFormat.SubjectPublicKeyInfo)).decode()
    unsigned = header + "." + url64(compact({
        "iss": "sculith-independent-fixture", "exp": int(time.time()) + 600, "cpk": cpk,
    }))
    assertion = {
        "token": unsigned + "." + sign(unsigned),
        "fingerprints": header + ".." + sign(header + "." + url64(fingerprint_payload(sdp))),
    }
    envelope = {
        "idp": {"domain": "sculith-independent-fixture", "protocol": "default"},
        "assertion": compact(assertion).decode(),
    }
    value = base64.b64encode(compact(envelope)).decode()
    return re.sub(r"^m=", "a=identity:" + value + "\r\nm=", sdp, count=1, flags=re.M)


def login_request(key, address):
    public = base64.b64encode(key.public_key().public_bytes(
        serialization.Encoding.DER, serialization.PublicFormat.SubjectPublicKeyInfo)).decode()

    def token(claims):
        unsigned = url64(compact({"alg": "ES384", "x5u": public})) + "." + url64(compact(claims))
        r, s = decode_dss_signature(key.sign(unsigned.encode(), ec.ECDSA(hashes.SHA384())))
        return unsigned + "." + url64(r.to_bytes(48, "big") + s.to_bytes(48, "big"))

    now = int(time.time())
    identity = token({
        "certificateAuthority": True, "identityPublicKey": public,
        "iat": now, "nbf": now - 60, "exp": now + 600,
        "extraData": {"XUID": "", "displayName": "SculithInterop", "identity": str(uuid.uuid4())},
    })
    auth = compact({"AuthenticationType": 2, "Certificate": compact({"chain": [identity]}).decode(), "Token": ""})
    client = token({
        "DeviceOS": 7, "GameVersion": "1.26.60", "LanguageCode": "en_US",
        "ServerAddress": address.split("://", 1)[-1], "SkinId": "sculith-original-test-skin",
        "SkinImageHeight": 64, "SkinImageWidth": 64,
        "SkinData": base64.b64encode(bytes([141, 110, 76, 255]) * 4096).decode(),
        "SkinResourcePatch": base64.b64encode(b'{"geometry":{"default":"geometry.humanoid.custom"}}').decode(),
    }).encode()
    return struct.pack("<I", len(auth)) + auth + struct.pack("<I", len(client)) + client


def varint(value):
    result = bytearray()
    while value >= 128:
        result.append((value & 127) | 128)
        value >>= 7
    return bytes(result) + bytes([value])


def read_varint(data, offset=0):
    value = 0
    for index in range(5):
        byte = data[offset + index]
        value |= (byte & 127) << (7 * index)
        if not byte & 128:
            return value, offset + index + 1
    raise AssertionError("oversized VarInt")


def decode_batch(data):
    packets = []
    while data:
        length, offset = read_varint(data)
        assert 0 < length <= len(data) - offset
        packets.append(data[offset:offset + length])
        data = data[offset + length:]
    return packets


def verify_answer(sdp):
    value = re.search(r"^a=identity:([^\r\n]+)", sdp, re.M).group(1)
    assertion = json.loads(json.loads(base64.b64decode(value))["assertion"])
    token = assertion["token"]
    header, payload, signature = token.split(".")
    claims = json.loads(un64(payload))
    assert claims["exp"] > time.time(), "expired server assertion"
    key = serialization.load_der_public_key(base64.b64decode(claims["cpk"]))
    assert isinstance(key, ec.EllipticCurvePublicKey) and key.curve.name == "secp384r1"

    def verify(header, payload, signature):
        assert json.loads(un64(header))["alg"] == "ES384"
        raw = un64(signature)
        assert len(raw) == 96
        der = encode_dss_signature(int.from_bytes(raw[:48], "big"), int.from_bytes(raw[48:], "big"))
        key.verify(der, (header + "." + payload).encode(), ec.ECDSA(hashes.SHA384()))

    verify(header, payload, signature)
    header, empty, signature = assertion["fingerprints"].split(".")
    assert empty == "", "fingerprint signature must be detached"
    verify(header, url64(fingerprint_payload(sdp)), signature)


def exchange(address, offer):
    request = urllib.request.Request(address + "/v1/join/independent-fixture",
        data=offer.encode(), headers={"Content-Type": "application/sdp"})
    with urllib.request.urlopen(request, timeout=3) as response:
        assert response.status == 200
        assert response.headers.get_content_type() == "application/sdp"
        data = response.read(65537)
        assert len(data) <= 65536
        return data.decode()


async def probe(address):
    pc = RTCPeerConnection(RTCConfiguration(iceServers=[]))
    received = asyncio.Event()
    both_open = asyncio.Event()
    data = []
    messages = asyncio.Queue()
    opened = set()
    reliable = pc.createDataChannel("ReliableDataChannel", ordered=True)
    unreliable = pc.createDataChannel("UnreliableDataChannel", ordered=False, maxRetransmits=0)

    @reliable.on("open")
    def reliable_open():
        opened.add("reliable")
        if len(opened) == 2:
            both_open.set()
        # Independent wire: NetherNet final-segment header, batch length,
        # RequestNetworkSettings ID 193 and BE i32 protocol 2225.
        reliable.send(bytes([0, 6, 0xC1, 1, 0, 0, 8, 0xB1]))

    @unreliable.on("open")
    def unreliable_open():
        opened.add("unreliable")
        if len(opened) == 2:
            both_open.set()

    @reliable.on("message")
    def message(value):
        data.append(value)
        messages.put_nowait(value)
        received.set()

    try:
        key = ec.generate_private_key(ec.SECP384R1())
        await pc.setLocalDescription(await pc.createOffer())
        assert "a=candidate:" in pc.localDescription.sdp, "client gathered no host candidate"
        answer = await asyncio.to_thread(exchange, address, sign_offer(pc.localDescription.sdp, key))
        verify_answer(answer)
        clean = re.sub(r"^a=identity:[^\r\n]*\r?\n", "", answer, flags=re.M)
        await pc.setRemoteDescription(RTCSessionDescription(sdp=clean, type="answer"))
        await asyncio.wait_for(received.wait(), timeout=12)
        await asyncio.wait_for(both_open.wait(), timeout=3)
        assert opened == {"reliable", "unreliable"}, opened
        # NetworkSettings ID 143; threshold 256 LE, zlib algorithm 0 LE.
        assert isinstance(data[0], bytes) and data[0][:8] == bytes([0, 12, 143, 1, 0, 1, 0, 0]), data
        assert len(data[0]) == 14
        assert await messages.get() == data[0]
        request = login_request(key, address)
        packet = b"\x01" + struct.pack(">i", 2225) + varint(len(request)) + request
        batch = varint(len(packet)) + packet
        compressor = zlib.compressobj(wbits=-15)
        batch = b"\x00" + compressor.compress(batch) + compressor.flush()
        assert len(batch) <= 10000
        reliable.send(b"\x00" + batch)
        packets = []
        while not packets:
            frame = await asyncio.wait_for(messages.get(), timeout=5)
            assert isinstance(frame, bytes) and frame[0] == 0
            compressed = frame[1:]
            assert compressed[0] in (0, 255)
            raw = zlib.decompress(compressed[1:], wbits=-15) if compressed[0] == 0 else compressed[1:]
            packets.extend(decode_batch(raw))
        assert packets.pop(0) == b"\x02\x00\x00\x00\x00", "expected direct LoginSuccess over DTLS, not a RakNet cipher handshake"
        while not packets:
            frame = await asyncio.wait_for(messages.get(), timeout=5)
            assert frame[0] == 0
            raw = zlib.decompress(frame[2:], wbits=-15) if frame[1] == 0 else frame[2:]
            packets.extend(decode_batch(raw))
        packet_id, _ = read_varint(packets[0])
        assert packet_id == 6, "login did not advance to resource-pack negotiation"
    finally:
        await asyncio.wait_for(pc.close(), timeout=3)


def smoke(server):
    versions = json.loads(subprocess.check_output([str(server), "versions"], text=True))
    assert versions["bedrock"]["protocol"] == 2225, "update this independent wire probe with the adapter"
    with tempfile.TemporaryDirectory(prefix="sculith-webrtc-") as temporary:
        root = Path(temporary)
        settings = root / "server.toml"
        settings.write_text(
            '[bedrock]\nsessions = true\nrequire_authentication = false\nlogin_timeout_secs = 15\n'
            '[network.java]\nenabled = false\nbind = "127.0.0.1:0"\n'
            '[network.bedrock]\nbind = "127.0.0.1:0"\n', encoding="utf-8")
        log_path = root / "server.log"
        with log_path.open("w", encoding="utf-8") as log:
            process = subprocess.Popen(
                [str(server), "run", "--config", str(settings)], cwd=ROOT, stdout=log, stderr=log,
                env=dict(os.environ, NO_COLOR="1", RUST_LOG="info,sculith::protocol::bedrock=debug"),
                creationflags=subprocess.CREATE_NEW_PROCESS_GROUP if os.name == "nt" else 0,
            )
            try:
                deadline = time.monotonic() + 5
                while time.monotonic() < deadline:
                    text = re.sub(r"\x1b\[[0-9;]*m", "", log_path.read_text(encoding="utf-8"))
                    match = re.search(r"NetherNet HTTP signaling listening.*?127\.0\.0\.1:(\d+)", text)
                    if match:
                        break
                    assert process.poll() is None, text
                    time.sleep(.05)
                else:
                    raise TimeoutError(text)
                # This UDP-only fixture needs no Proactor subprocess/pipe I/O.
                # Windows Proactor teardown may leave aioice waiting forever
                # for a closed datagram transport after a successful login.
                # Keep the existing connection assertions and close deadline.
                with asyncio.Runner(loop_factory=asyncio.SelectorEventLoop) as runner:
                    runner.run(asyncio.wait_for(probe("http://127.0.0.1:" + match[1]), timeout=20))
                process.send_signal(signal.CTRL_BREAK_EVENT if os.name == "nt" else signal.SIGINT)
                assert process.wait(timeout=5) == 0
            except Exception:
                print(log_path.read_text(encoding="utf-8"))
                raise
            finally:
                if process.poll() is None:
                    process.kill()
                    process.wait(timeout=5)
    print("PASS: independent aiortc/OpenSSL ICE, DTLS, SCTP, both channels, signed operator identity, Bedrock network settings and transport-bound login without an application-cipher handshake")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--server", type=Path, default=ROOT / ("target/debug/sculith.exe" if os.name == "nt" else "target/debug/sculith"))
    args = parser.parse_args()
    smoke(args.server.resolve(strict=True))
