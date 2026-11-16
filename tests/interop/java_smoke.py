#!/usr/bin/env python3
"""Independent byte-level protocol-777 client; not official Minecraft rendering."""
import argparse
from collections import deque
import hashlib
import json
import os
from pathlib import Path
import re
import signal
import socket
import struct
import subprocess
import tempfile
import time
import uuid

ROOT = Path(__file__).resolve().parents[2]


def vi(value):
    value &= 0xffffffff
    out = bytearray()
    while value >= 128:
        out.append((value & 127) | 128)
        value >>= 7
    return bytes(out) + bytes([value])


def string(value):
    data = value.encode()
    return vi(len(data)) + data


def pos(x, y, z):
    return struct.pack(">Q", ((x & 0x3ffffff) << 38) | ((z & 0x3ffffff) << 12) | (y & 4095))


class Read:
    def __init__(self, data):
        self.data = memoryview(data)
        self.i = 0

    def take(self, count):
        assert 0 <= count <= len(self.data) - self.i, "truncated Java field"
        value = self.data[self.i:self.i + count]
        self.i += count
        return value.tobytes()

    def number(self, fmt):
        return struct.unpack(fmt, self.take(struct.calcsize(fmt)))[0]

    def varint(self):
        value = 0
        for i in range(5):
            byte = self.number(">B")
            assert i != 4 or byte & 0xf0 == 0
            value |= (byte & 127) << (7 * i)
            if not byte & 128:
                return value - 2**32 if value & 2**31 else value
        raise AssertionError("oversized VarInt")

    def string(self):
        n = self.varint()
        assert 0 <= n <= 32767
        return self.take(n).decode()

    def end(self):
        assert self.i == len(self.data), f"{len(self.data) - self.i} trailing Java bytes"


class BufferedSocket:
    def __init__(self, inner):
        self.inner = inner
        self.frames = deque()

    def __getattr__(self, name):
        return getattr(self.inner, name)


def receive(sock):
    frames = getattr(sock, "frames", None)
    if frames:
        return frames.popleft()
    return receive_network(sock)


def receive_network(sock):
    def exact(count):
        result = bytearray()
        while len(result) < count:
            data = sock.recv(count - len(result))
            assert data, "Java server closed the connection"
            result.extend(data)
        return bytes(result)

    prefix = bytearray()
    for _ in range(5):
        prefix.extend(exact(1))
        if not prefix[-1] & 128:
            break
    length = Read(prefix).varint()
    assert 0 < length <= 1024 * 1024
    reader = Read(exact(length))
    return reader.varint(), reader


def send(sock, id_, data=b""):
    data = vi(id_) + data
    assert len(data) <= 8192
    sock.sendall(vi(len(data)) + data)


def client_information():
    return string("en_us") + bytes([8, 0, 1, 127, 1, 0, 1, 0])


def palette(reader, size):
    bits = reader.number(">B")
    if bits == 0:
        return [reader.varint()] * size
    assert 4 <= bits <= 8
    entries = [reader.varint() for _ in range(reader.varint())]
    assert 1 <= len(entries) <= 16
    per = 64 // bits
    words = [reader.number(">Q") for _ in range((size + per - 1) // per)]
    return [entries[(words[i // per] >> ((i % per) * bits)) & ((1 << bits) - 1)]
            for i in range(size)]


def light(reader, edited=None):
    masks = [int.from_bytes(reader.take(reader.varint()), "little") for _ in range(4)]
    assert masks == [2**26 - 1, 0, 0, 2**26 - 1], masks
    assert reader.varint() == 26
    for section in range(-5, 21):
        length = reader.varint()
        assert length == 2048
        values = reader.take(length)
        for y in range(16):
            if edited is None:
                expected = bytes([255 if section * 16 + y >= 4 else 0]) * 128
                assert values[y * 128:(y + 1) * 128] == expected
            elif section == 0 and y == 3:
                offset = y * 128 + 1 * 8
                # Column (1,1) becomes directly lit after removing its grass.
                assert (values[offset] >> 4) == (15 if edited == "air" else 0)
    assert reader.varint() == 0


def chunk(reader, edits=None):
    x, z = reader.number(">i"), reader.number(">i")
    assert -2 <= x <= 2 and -2 <= z <= 2
    count = reader.varint()
    assert count == 2
    for expected in [1, 4]:
        assert reader.varint() == expected
        assert reader.varint() == 37
        words = [reader.number(">Q") for _ in range(37)]
        assert all(((words[i // 7] >> ((i % 7) * 9)) & 511) == 68 for i in range(256))
    sections = Read(reader.take(reader.varint()))
    for sy in range(-4, 20):
        blocks, fluids = sections.number(">H"), sections.number(">H")
        values = palette(sections, 4096)
        assert fluids == 0 and blocks == sum(v != 0 for v in values)
        for i, value in enumerate(values):
            y = sy * 16 + i // 256
            lx, lz = i % 16, (i // 16) % 16
            wanted = 88 if y == 0 else 10 if y in (1, 2) else 9 if y == 3 else 0
            if edits and (x, z) == (0, 0):
                wanted = edits.get((lx, y, lz), wanted)
            assert value == wanted, (x, z, lx, y, lz, value, wanted)
        assert sections.number(">B") == 0 and sections.varint() == 41
    sections.end()
    assert reader.varint() == 0
    light(reader)
    reader.end()
    return x, z


def item_stack(r):
    count=r.varint();assert 0<=count<=64
    if not count:return (0,0)
    item=r.varint();assert item in (1,55,54,87)
    assert r.varint()==0 and r.varint()==0
    return (count,item)


def inventory_content(client,r):
    assert r.varint()==0
    client.inventory_state=r.varint();assert 0<=client.inventory_state<=32767
    assert r.varint()==46
    client.menu=[item_stack(r) for _ in range(46)]
    client.cursor=item_stack(r)
    r.end()


def consume_peer(client, id_, r):
    """Validate one public peer frame; preserve private presentation traffic."""
    if not hasattr(client,"peer_profiles"):
        client.peer_profiles={};client.peer_entities={}
    if id_==18:
        inventory_content(client,r);return True
    if id_==98:
        client.cursor=item_stack(r);r.end();return True
    if id_==45:
        send(client.socket,28,r.take(8));r.end();return True
    if id_==71:
        assert r.number(">B")==0xad
        count=r.varint();assert 0<count<=64
        for _ in range(count):
            uuid_=r.take(16);name=r.string()
            assert 0<len(name)<=16 and all(c.isascii() and (c.isalnum() or c=="_") for c in name)
            assert r.varint()==0 and r.varint()==1 and r.take(2)==bytes([1,1])
            assert r.number(">B")==8
            raw=r.take(r.number(">H"));assert not any(b>=0xf0 for b in raw)
            display=raw.decode("utf-8","surrogatepass").encode("utf-16","surrogatepass").decode("utf-16")
            assert display.strip() and len(display.encode())<=64 and r.number(">B")==1
            client.peer_profiles[uuid_]={'name':name,'display':display}
        r.end();assert len(client.peer_profiles)<=64;return True
    if id_==70:
        count=r.varint();assert 0<count<=64
        for _ in range(count):
            uuid_=r.take(16);assert uuid_ in client.peer_profiles
            assert not any(entity['uuid']==uuid_ for entity in client.peer_entities.values())
            del client.peer_profiles[uuid_]
        r.end();return True
    if id_==1:
        start=r.i;entity=r.varint();uuid_=r.take(16);type_=r.varint()
        if type_!=159:r.i=start;return False
        assert entity>0 and entity!=client.entity_id and uuid_ in client.peer_profiles
        assert entity not in client.peer_entities
        position=[r.number(">d") for _ in range(3)]
        assert all(abs(p)<=30_000_000 for p in position)
        assert r.number(">B")==0
        rotation=r.take(3);assert r.varint()==0;r.end()
        client.peer_entities[entity]={'uuid':uuid_,'position':position,'rotation':rotation};return True
    if id_==104:
        start=r.i;entity=r.varint()
        if entity not in client.peer_entities:r.i=start;return False
        assert r.number(">B")==0
        count=r.varint();assert 0<=count<=64
        item=0
        if count:
            item=r.varint();assert item in (1,55,54,87)
            assert r.take(2)==bytes(2)
        r.end();client.peer_entities[entity]['held']=(count,item);return True
    if id_==35:
        start=r.i;entity=r.varint()
        if entity not in client.peer_entities:r.i=start;return False
        assert r.varint()==0
        position=[r.number(">d") for _ in range(3)]
        assert all(abs(p)<=30_000_000 for p in position)
        yaw,pitch=r.number(">f"),r.number(">f")
        assert -180<=yaw<=180 and -90<=pitch<=90 and r.number(">B")==0;r.end()
        client.peer_entities[entity]['position']=position
        client.peer_entities[entity]['yaw']=yaw;client.peer_entities[entity]['pitch']=pitch;return True
    if id_==85:
        start=r.i;entity=r.varint()
        if entity not in client.peer_entities:r.i=start;return False
        client.peer_entities[entity]['head']=r.number(">B");r.end();return True
    if id_==78:
        start=r.i;count=r.varint();assert 0<count<=64
        ids=[r.varint() for _ in range(count)]
        if not all(entity in client.peer_entities for entity in ids):r.i=start;return False
        for entity in ids:del client.peer_entities[entity]
        r.end();return True
    return False


def command_tree(r):
    count=r.varint();assert 3<=count<=259
    nodes=[]
    for _ in range(count):
        flags=r.number('>B');assert flags in (0,5,6)
        children=[r.varint() for _ in range(r.varint())]
        assert len(set(children))==len(children) and all(0<=n<count for n in children)
        name=None
        if flags in (5,6):name=r.string()
        if flags==6:
            assert r.varint()==5 and r.varint()==2 # brigadier:string, greedy.
        nodes.append((flags,name,children))
    root=r.varint();assert 0<=root<count and nodes[root][0]==0
    names={};visited={root}
    for index in nodes[root][2]:
        flags,name,children=nodes[index];assert flags==5 and name not in names
        assert 1<=len(name)<=32 and all(c.isascii() and (c.islower() or c.isdigit() or c=='_') for c in name)
        assert len(children)==1
        arg=children[0];assert nodes[arg]==(6,'arguments',[])
        visited.update((index,arg));names[name]=True
    assert len(visited)==count and 'help' in names
    return names


def receive_play(client):
    while True:
        id_,r=receive(client.socket)
        if not consume_peer(client,id_,r):
            return id_,r


class Client:
    def __init__(self, address, name="SculithJavaProbe", edits=None,
                 acknowledged_position=(.5, 4, .5), pack_handler=None, inventory_fixture=True):
        self.socket = BufferedSocket(socket.create_connection(address, timeout=5))
        self.socket.settimeout(5)
        self.uuid = uuid.UUID(bytes=hashlib.md5(("OfflinePlayer:" + name).encode()).digest(), version=3)
        send(self.socket, 0, vi(777) + string("localhost") + struct.pack(">H", address[1]) + vi(2))
        send(self.socket, 0, string(name) + bytes(16))
        id_, r = receive(self.socket)
        assert id_ == 2, (id_, r.take(len(r.data) - r.i))
        assert uuid.UUID(bytes=r.take(16)) == self.uuid and r.string() == name
        assert r.varint() == 0 and uuid.UUID(bytes=r.take(16)).int != 0
        r.end()
        send(self.socket, 3)
        send(self.socket, 0, client_information())
        registries = {}
        tag_count = 0
        for _ in range(40):
            id_, r = receive(self.socket)
            if id_ == 0x0d:
                assert r.varint() == 1 and r.string() == "minecraft:vanilla"
            elif id_ == 0x0f:
                assert r.varint() == 1 and [r.string() for _ in range(3)] == ["minecraft", "core", "26.3"]
                send(self.socket, 7, vi(1) + string("minecraft") + string("core") + string("26.3"))
            elif id_ == 7:
                kind = r.string()
                assert kind not in registries
                entries = []
                for _ in range(r.varint()):
                    entries.append(r.string())
                    assert r.number(">B") == 0, "expected negotiated client-core data"
                registries[kind] = entries
            elif id_ == 0x0e:
                for _ in range(r.varint()):
                    r.string()
                    for _ in range(r.varint()):
                        r.string()
                        tag_count += 1
                        ids = [r.varint() for _ in range(r.varint())]
                        assert all(v >= 0 for v in ids)
            elif id_ == 3:
                r.end()
                assert len(registries) == 32 and tag_count == 773
                assert registries["minecraft:dimension_type"][0] == "minecraft:overworld"
                assert registries["minecraft:worldgen/biome"][41] == "minecraft:plains"
                send(self.socket, 3)
                break
            elif id_ == 9:
                assert pack_handler is not None, "unexpected resource pack offer"
                pack_handler(r, self.socket)
            else:
                raise AssertionError(("unexpected configuration packet", id_))
            r.end()
        else:
            raise AssertionError("configuration not completed")
        chunks = set()
        inventory = {}
        for _ in range(128):
            id_, r = receive(self.socket)
            if id_ == 0x32:
                self.entity_id = r.number(">i")
                assert self.entity_id > 0 and r.number(">B") == 0
                assert r.varint() == 1 and r.string() == "sculith:world"
                assert [r.varint() for _ in range(3)] == [20, 2, 2]
                assert r.take(3) == bytes([0, 1, 0])
                assert r.varint() == 0 and r.string() == "sculith:world"
                assert r.number(">q") == 0
                # 26.3 uses GameType VarInt and previous OPTIONAL_VAR_INT.
                # The legacy -1 byte consumed the next field in the real client.
                assert [r.varint(), r.varint()] == [1, 0]
                assert r.take(3) == bytes([0, 1, 0])
                assert [r.varint(), r.varint()] == [0, 63] and r.take(2) == bytes(2)
            elif id_ == 0x41:
                assert r.number(">B") == 15
                assert abs(r.number(">f") - .05) < 1e-6 and abs(r.number(">f") - .1) < 1e-6
            elif id_ == 16:
                self.commands=command_tree(r)
            elif id_ in (0x61, 0x71):
                assert r.varint() == 2
            elif id_ == 0x63:
                assert r.string() == "sculith:world" and r.take(8) == pos(0, 4, 0)
                assert r.take(8) == bytes(8)
            elif id_ == 0x73:
                assert r.number(">q") == 0
                assert [r.varint() for _ in range(3)] == [1, 0, 6000]
                assert r.take(8) == bytes(8)
            elif id_ == 0x6e:
                slot, count = r.varint(), r.varint()
                if count:
                    inventory[slot] = (count, r.varint())
                    assert r.take(2) == bytes(2)
                else:
                    inventory[slot] = (0, 0)
            elif id_ == 0x6b:
                assert r.varint() == 0
            elif id_ == 18:
                inventory_content(self,r)
                assert all(item == (0, 0) for item in self.menu)
            elif id_ == 98:
                self.cursor=item_stack(r);assert self.cursor==(0,0)
            elif id_ == 0x27:
                assert r.number(">B") == 13 and r.number(">f") == 0
            elif id_ == 0x60:
                assert [r.varint(), r.varint()] == [0, 0]
            elif id_ == 0x0c:
                pass
            elif id_ == 0x2e:
                point = chunk(r, edits)
                assert point not in chunks
                chunks.add(point)
            elif id_ == 0x0b:
                assert r.varint() == 25 and len(chunks) == 25
                send(self.socket, 0x0b, struct.pack(">f", 8))
            elif id_ == 0x49:
                teleport = r.varint()
                assert [r.number(">d") for _ in range(6)] == [.5, 4, .5, 0, 0, 0]
                assert r.take(12) == bytes(12)
                r.end()
                assert len(chunks) == 25 and len(inventory) == 41
                assert all(item == (0, 0) for item in inventory.values())
                send(self.socket, 0, vi(teleport) + struct.pack(">dddff", *acknowledged_position, 0, 0))
                send(self.socket, 0x2c)
                if inventory_fixture:
                    self.seed_inventory()
                return
            else:
                raise AssertionError(("unexpected Java spawn packet", id_, r.take(len(r.data) - r.i)))
            r.end()
        raise AssertionError("Java spawn not completed")

    def seed_inventory(self):
        expected = {slot: (64, item) for slot, item in enumerate((1, 55, 54))}
        for slot, (_, item) in expected.items():
            send(self.socket, 0x39, struct.pack(">h", 36 + slot) + vi(64) + vi(item) + bytes(2))
        pending_bytes = 0
        for _ in range(128):
            id_, reader = receive_network(self.socket)
            if id_ == 0x6e:
                slot = reader.varint()
                stack = item_stack(reader)
                reader.end()
                assert expected.pop(slot) == stack
                self.menu[36 + slot] = stack
                if not expected:
                    return
            else:
                pending_bytes += len(reader.data)
                assert pending_bytes <= 256 * 1024, "fixture traffic budget exceeded"
                self.socket.frames.append((id_, reader))
        raise AssertionError("creative inventory fixture was not acknowledged")

    def block(self, expected_position, expected_state, edited):
        updated = False
        for _ in range(12):
            id_, r = receive_play(self)
            if id_ == 4:
                r.varint()
            elif id_ == 8:
                assert r.take(8) == pos(*expected_position) and r.varint() == expected_state
                updated = True
            elif id_ == 0x31:
                assert [r.varint(), r.varint()] == [0, 0]
                light(r, edited)
                assert updated, "light update preceded its block commit"
                r.end()
                return
            else:
                raise AssertionError(("unexpected block response", id_))
            r.end()
        raise AssertionError("authoritative block update missing")


def smoke(server):
    with tempfile.TemporaryDirectory(prefix="sculith-java-") as temporary:
        root = Path(temporary)
        config = root / "server.toml"
        config.write_text(
            '[java]\nsessions = true\nrequire_authentication = false\n'
            '[network.java]\nbind = "127.0.0.1:0"\n'
            '[network.bedrock]\nenabled = false\nbind = "127.0.0.1:0"\n', encoding="utf-8")
        log_path = root / "server.log"
        with log_path.open("w", encoding="utf-8") as output:
            process = subprocess.Popen([str(server), "run", "--config", str(config)],
                cwd=ROOT, stdout=output, stderr=output,
                env=dict(os.environ, NO_COLOR="1", RUST_LOG="info,sculith::protocol::java=debug"),
                creationflags=subprocess.CREATE_NEW_PROCESS_GROUP if os.name == "nt" else 0)
            try:
                deadline = time.monotonic() + 5
                while time.monotonic() < deadline:
                    text = re.sub(r"\x1b\[[0-9;]*m", "", log_path.read_text(encoding="utf-8"))
                    match = re.search(r"Java listening.*?127\.0\.0\.1:(\d+)", text)
                    if match:
                        break
                    assert process.poll() is None, text
                    time.sleep(.05)
                else:
                    raise TimeoutError(text)
                fresh = Client(("127.0.0.1", int(match[1])), "EmptyStartProbe", inventory_fixture=False)
                assert all(item == (0, 0) for item in fresh.menu) and fresh.cursor == (0, 0)
                fresh.socket.close()
                client = Client(("127.0.0.1", int(match[1])))
                send(client.socket, 0x1f, struct.pack(">dddffB", 1, 4, .5, 0, 0, 1))
                send(client.socket, 0x29, vi(0) + pos(1, 3, 1) + b"\x01" + vi(1))
                client.block((1, 3, 1), 0, "air")
                send(client.socket, 0x42, vi(0) + pos(1, 2, 1) + vi(1)
                     + struct.pack(">fffBB", .5, 1, .5, 0, 0) + vi(2))
                client.block((1, 3, 1), 1, "stone")
                # Replaying sequence 2 at a new target must not create another block.
                send(client.socket, 0x42, vi(0) + pos(1, 3, 1) + vi(1)
                     + struct.pack(">fffBB", .5, 1, .5, 0, 0) + vi(2))
                id_, r = receive_play(client)
                assert id_ == 4 and r.varint() == 2
                r.end()
                # A large teleport is corrected to the owner position.
                send(client.socket, 0x1f, struct.pack(">dddffB", 1000, 4, .5, 0, 0, 1))
                id_, r = receive_play(client)
                assert id_ == 0x49, "replayed block sequence changed the world"
                correction = r.varint()
                assert [r.number(">d") for _ in range(6)] == [1, 4, .5, 0, 0, 0]
                assert r.take(12) == bytes(12)
                r.end()
                send(client.socket, 0x29, vi(0) + pos(2, 3, 1) + b"\x01" + vi(3))
                id_, r = receive_play(client)
                assert id_ == 4 and r.varint() == 3
                r.end()
                id_, r = receive_play(client)
                assert id_ == 8 and r.take(8) == pos(2, 3, 1) and r.varint() == 9
                r.end()
                send(client.socket, 0, vi(correction) + struct.pack(">dddff", 1, 4, .5, 0, 0))
                # Select the existing server dirt slot, then rebuild the shared column.
                send(client.socket, 0x36, struct.pack(">h", 1))
                send(client.socket, 0x29, vi(0) + pos(1, 3, 1) + b"\x01" + vi(4))
                client.block((1, 3, 1), 0, "air")
                send(client.socket, 0x42, vi(0) + pos(1, 2, 1) + vi(1)
                     + struct.pack(">fffBB", .5, 1, .5, 0, 0) + vi(5))
                client.block((1, 3, 1), 10, "stone")
                # Client-proposed offhand stacks cannot replace server inventory.
                send(client.socket, 0x39, struct.pack(">h", 45) + vi(64) + vi(1) + b"\x00\x00")
                for slot in range(41):
                    id_, r = receive_play(client)
                    assert id_ == 0x6e and r.varint() == slot
                    count = r.varint()
                    if slot < 3:
                        assert count == 64 and r.varint() == [1, 55, 54][slot] and r.take(2) == bytes(2)
                    else:
                        assert count == 0
                    r.end()
                id_, r = receive_play(client)
                assert id_ == 0x6b and r.varint() == 1
                r.end()
                client.socket.close()
                # A second session sees the committed dirt and no replay/pending edits.
                reader = Client(("127.0.0.1", int(match[1])), "JavaProbeReader", {(1, 3, 1): 10})
                reader.socket.close()
                # Confirmation data cannot authorize another position, including NaN.
                for name, position in [("WrongAck", (1000, 4, .5)), ("NonfiniteAck", (float("nan"), 4, .5))]:
                    invalid = Client(("127.0.0.1", int(match[1])), name,
                                     {(1, 3, 1): 10}, acknowledged_position=position, inventory_fixture=False)
                    id_, r = receive_play(invalid)
                    assert id_ == 0x20 and r.number(">B") == 8
                    assert "request rejected" in r.take(r.number(">H")).decode()
                    r.end()
                    invalid.socket.close()
                assert "Java client acknowledged the shared flat-world spawn" in log_path.read_text(encoding="utf-8")
                process.send_signal(signal.CTRL_BREAK_EVENT if os.name == "nt" else signal.SIGINT)
                assert process.wait(timeout=5) == 0
            except Exception:
                print(log_path.read_text(encoding="utf-8"))
                raise
            finally:
                if process.poll() is None:
                    process.kill()
                    process.wait(timeout=5)
    print("PASS: independent Java 26.3 login/configuration, all spawn voxels, lighting, player initialization, authoritative movement/building, replay/pending-teleport rejection, inventory correction and shared state after reconnect")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--server", type=Path, default=ROOT / ("target/debug/sculith.exe" if os.name == "nt" else "target/debug/sculith"))
    args = parser.parse_args()
    smoke(args.server.resolve(strict=True))
