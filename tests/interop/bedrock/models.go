package main

import (
	"fmt"
	"net"
	"sync"
	"time"

	"github.com/go-gl/mathgl/mgl32"
	"github.com/sandertv/gophertunnel/minecraft"
	"github.com/sandertv/gophertunnel/minecraft/nbt"
	"github.com/sandertv/gophertunnel/minecraft/protocol"
	"github.com/sandertv/gophertunnel/minecraft/protocol/packet"
)

type modelRegistry struct {
	sync.Mutex
	seen  bool
	model bool
	err   error
}

func (r *modelRegistry) observe(header packet.Header, payload []byte, _, _ net.Addr) {
	if header.PacketID != packet.IDAvailableActorIdentifiers {
		return
	}
	r.Lock()
	defer r.Unlock()
	if r.seen {
		r.err = fmt.Errorf("duplicate actor registry")
		return
	}
	r.seen = true
	var root map[string]any
	if err := nbt.UnmarshalEncoding(payload, &root, nbt.NetworkLittleEndian); err != nil {
		r.err = err
		return
	}
	actors, ok := root["idlist"].([]any)
	if !ok {
		r.err = fmt.Errorf("invalid actor registry: %#v", root)
		return
	}
	ids, runtimeIDs := map[string]bool{}, map[int32]bool{}
	armor, model := false, false
	for _, value := range actors {
		actor, ok := value.(map[string]any)
		if !ok {
			r.err = fmt.Errorf("invalid actor row: %#v", value)
			return
		}
		id, validID := actor["id"].(string)
		runtimeID, validRuntimeID := actor["rid"].(int32)
		if !validID || !validRuntimeID || ids[id] || runtimeIDs[runtimeID] {
			r.err = fmt.Errorf("invalid or duplicate actor identity: %#v", actor)
			return
		}
		ids[id], runtimeIDs[runtimeID] = true, true
		if id == "minecraft:armor_stand" {
			armor = runtimeID == 61
		}
		if id == "sculith:native_statue" {
			model = runtimeID >= 100000 && actor["bid"] == "" && actor["summonable"] == byte(1) && actor["hasspawnegg"] == byte(0)
		}
	}
	r.model = ids["sculith:native_statue"]
	if !armor || ids["sculith:demo_statue"] || (r.model && !model) {
		r.err = fmt.Errorf("native model registry did not preserve vanilla IDs and mapped identity")
	}
}

func (r *modelRegistry) check(expectedModel bool) error {
	r.Lock()
	defer r.Unlock()
	if !r.seen {
		return fmt.Errorf("missing actor registry before world spawn")
	}
	if r.model != expectedModel {
		return fmt.Errorf("unexpected custom model registration: %v", r.model)
	}
	return r.err
}

func checkUnavailableModel(c *minecraft.Conn) error {
	c.SetReadDeadline(time.Now().Add(2 * time.Second))
	for n := 0; n < 8192; n++ {
		pk, err := c.ReadPacket()
		if err != nil {
			if timeout, ok := err.(net.Error); ok && timeout.Timeout() {
				fmt.Println("PASS: unavailable model has no native actor or fallback")
				return nil
			}
			return err
		}
		if actor, ok := pk.(*packet.AddActor); ok && (actor.EntityType == "sculith:native_statue" || actor.EntityType == "minecraft:armor_stand") {
			return fmt.Errorf("unavailable model leaked a client actor: %#v", actor)
		}
	}
	return fmt.Errorf("unavailable-model observation limit exceeded")
}

func checkModels(c *minecraft.Conn, snapshot bool) error {
	var runtimeID uint64
	var uniqueID int64
	stage := 0
	c.SetReadDeadline(time.Now().Add(20 * time.Second))
	for n := 0; n < 8192; n++ {
		pk, err := c.ReadPacket()
		if err != nil {
			return err
		}
		switch v := pk.(type) {
		case *packet.AddActor:
			if v.EntityType != "sculith:native_statue" {
				continue
			}
			scale, ok := v.EntityMetadata[protocol.EntityDataKeyScale].(float32)
			if !ok {
				return fmt.Errorf("missing scale: %#v", v.EntityMetadata)
			}
			flags, ok := v.EntityMetadata[protocol.EntityDataKeyFlags].(int64)
			if !ok || flags&(1<<protocol.EntityDataFlagHasGravity|1<<protocol.EntityDataFlagHasCollision) != 0 || flags&(1<<protocol.EntityDataFlagNoAI) == 0 {
				return fmt.Errorf("invalid static actor flags: %v", flags)
			}
			position, expectedScale := mgl32.Vec3{1, -60, 4}, float32(1.25)
			if snapshot {
				position, expectedScale = mgl32.Vec3{3, -60, 4}, 2
			}
			if v.Position != position || scale != expectedScale || runtimeID != 0 || v.EntityRuntimeID == 0 {
				return fmt.Errorf("invalid model spawn: %#v", v)
			}
			runtimeID, uniqueID, stage = v.EntityRuntimeID, v.EntityUniqueID, 1
			fmt.Printf("MODEL_READY id=%d scale=%v position=%v\n", runtimeID, scale, position)
			if snapshot {
				return nil
			}
		case *packet.SetActorData:
			if v.EntityRuntimeID != runtimeID || stage != 1 {
				continue
			}
			if scale, ok := v.EntityMetadata[protocol.EntityDataKeyScale].(float32); ok && scale == 2 {
				stage = 2
				fmt.Println("MODEL_SCALED")
			}
		case *packet.MoveActorAbsolute:
			if v.EntityRuntimeID != runtimeID {
				continue
			}
			if stage != 2 || v.Position != (mgl32.Vec3{3, -60, 4}) || v.Flags&packet.MoveFlagTeleport == 0 {
				return fmt.Errorf("invalid model move: stage=%d %#v", stage, v)
			}
			stage = 3
			fmt.Println("MODEL_MOVED")
		case *packet.RemoveActor:
			if v.EntityUniqueID != uniqueID || runtimeID == 0 {
				continue
			}
			if stage != 3 {
				return fmt.Errorf("premature model removal at stage %d", stage)
			}
			fmt.Println("PASS: shared model spawn/scale/move/remove and static actor metadata")
			return nil
		}
	}
	return fmt.Errorf("shared model lifecycle incomplete at stage %d", stage)
}
