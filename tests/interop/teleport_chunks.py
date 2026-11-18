"""A stationary Java client receives the new view after a server teleport."""

import struct
import time

from pick_events import CB, SB, PickJava
from shared_resources import varint


def confirm_position(client, fields):
    teleport = varint(fields.vi())
    coords = fields.take(24)
    fields.take(24)
    client.send(SB["accept_teleportation"], teleport + coords + fields.take(8))
    return struct.unpack(">ddd", coords)


def check(server, port, root):
    name = "TeleportJava"
    destination = (512.5, -58.0, 768.5)
    center = (32, 48)
    client = PickJava(port, name)
    try:
        client.join_with_resources(port, root, False)
        client.send(SB["player_loaded"])
        client.socket.settimeout(10)
        for _ in range(8192):
            id_, fields = client.event()
            if id_ == CB["player_position"]:
                confirm_position(client, fields)
                break
        else:
            raise AssertionError("initial position missing before console teleport")

        # Let the server process the spawn confirmation before issuing a second teleport.
        nonce = struct.pack(">q", 713)
        client.send(SB["ping_request"], nonce)
        for _ in range(8192):
            id_, fields = client.event()
            if id_ == CB["pong_response"] and fields.take(8) == nonce:
                break
        else:
            raise AssertionError("initial teleport confirmation did not complete")

        x, y, z = destination
        server.stdin.write(f"tp {name} {x} {y} {z}\n")
        server.stdin.flush()
        deadline = time.monotonic() + 10
        confirmed = False
        watched = False
        columns = set()
        while time.monotonic() < deadline:
            # Leave movement packets to the other interop cases. Confirmations
            # and batch acknowledgements are enough for the destination view.
            client.socket.settimeout(max(.01, deadline - time.monotonic()))
            try:
                id_, fields = client.event()
            except TimeoutError:
                break
            if id_ == CB["player_position"]:
                confirmed |= confirm_position(client, fields) == destination
            elif id_ == CB["set_chunk_cache_center"]:
                watched |= (fields.vi(), fields.vi()) == center
            elif id_ == CB["level_chunk_with_light"]:
                position = struct.unpack(">ii", fields.take(8))
                if all(abs(a - b) <= 1 for a, b in zip(position, center)):
                    columns.add(position)
            if confirmed and watched and len(columns) == 9:
                return "same-world teleport delivered the new center and nine columns without client movement"
        raise AssertionError(("teleport destination view incomplete", confirmed, watched, sorted(columns)))
    finally:
        client.socket.close()
