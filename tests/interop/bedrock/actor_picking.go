package main

import (
	"bytes"
	"fmt"
	"net"
	"sync"
	"time"

	"github.com/sandertv/gophertunnel/minecraft"
	"github.com/sandertv/gophertunnel/minecraft/protocol"
	"github.com/sandertv/gophertunnel/minecraft/protocol/packet"
)

type actorPickRegistry struct {
	sync.Mutex
	near, far         int64
	seenNear, seenFar bool
	err               error
	types             []string
}

func (r *actorPickRegistry) observe(header packet.Header, payload []byte, _, _ net.Addr) {
	if header.PacketID != packet.IDAddActor {
		return
	}
	r.Lock()
	defer r.Unlock()
	defer func() {
		if err := recover(); err != nil {
			r.err = fmt.Errorf("actor decode: %v", err)
		}
	}()
	var actor packet.AddActor
	actor.Marshal(protocol.NewReader(bytes.NewBuffer(payload), 0, true))
	r.types = append(r.types, fmt.Sprintf("%s at %v", actor.EntityType, actor.Position))
	if actor.EntityType != "minecraft:pig" && actor.EntityType != "pig" {
		return
	}
	if actor.Position.X() < 6 {
		r.near, r.seenNear = actor.EntityUniqueID, true
	} else {
		r.far, r.seenFar = actor.EntityUniqueID, true
	}
}

func checkActorPicking(c *minecraft.Conn, actors *actorPickRegistry) error {
	c.SetReadDeadline(time.Now().Add(6 * time.Second))
	var near, far int64
	for n := 0; n < 4096; n++ {
		actors.Lock()
		near, far = actors.near, actors.far
		ready, err := actors.seenNear && actors.seenFar, actors.err
		actors.Unlock()
		if err != nil {
			return err
		}
		if ready {
			break
		}
		if _, err := c.ReadPacket(); err != nil {
			actors.Lock()
			types := append([]string(nil), actors.types...)
			actors.Unlock()
			return fmt.Errorf("waiting for test actors %v: %w", types, err)
		}
	}
	write := func(pk packet.Packet) error {
		if err := c.WritePacket(pk); err != nil {
			return err
		}
		return c.Flush()
	}
	const barrier = int64(0x0102030405060708)
	for _, id := range []int64{far, near + 1<<32} {
		if err := write(&packet.ActorPickRequest{EntityUniqueID: id, HotBarSlot: 9}); err != nil {
			return err
		}
	}
	if err := write(&packet.NetworkStackLatency{Timestamp: barrier, NeedsResponse: false}); err != nil {
		return err
	}
	c.SetReadDeadline(time.Now().Add(6 * time.Second))
	acknowledged := false
	for n := 0; n < 4096 && !acknowledged; n++ {
		pk, err := c.ReadPacket()
		if err != nil {
			return err
		}
		switch v := pk.(type) {
		case *packet.NetworkStackLatency:
			acknowledged = v.Timestamp == barrier && !v.NeedsResponse
		case *packet.InventoryContent:
			if v.WindowID == 0 {
				for _, item := range v.Content {
					if item.Stack.Count != 0 {
						return fmt.Errorf("out-of-range or truncated actor ID created an item")
					}
				}
			}
		case *packet.InventorySlot:
			if v.WindowID == 0 && v.NewItem.Stack.Count != 0 {
				return fmt.Errorf("rejected actor pick changed inventory")
			}
		}
	}
	if !acknowledged {
		return fmt.Errorf("missing rejection barrier")
	}
	for attempt := 0; attempt < 2; attempt++ {
		if attempt == 1 {
			if err := write(&packet.PlayerHotBar{SelectedHotBarSlot: 5, WindowID: 0, SelectHotBarSlot: true}); err != nil {
				return err
			}
		}
		if err := write(&packet.ActorPickRequest{EntityUniqueID: near, HotBarSlot: byte(9 - attempt)}); err != nil {
			return err
		}
		c.SetReadDeadline(time.Now().Add(6 * time.Second))
		selected, received := false, false
		for n := 0; n < 4096 && !received; n++ {
			pk, err := c.ReadPacket()
			if err != nil {
				return fmt.Errorf("actor pick attempt %d: %w", attempt, err)
			}
			switch v := pk.(type) {
			case *packet.PlayerHotBar:
				selected = v.WindowID == 0 && v.SelectHotBarSlot && v.SelectedHotBarSlot == 0
			case *packet.InventoryContent:
				if !selected || v.WindowID != 0 || len(v.Content) != 36 {
					continue
				}
				item := v.Content[0]
				name := ""
				for _, entry := range c.GameData().Items {
					if int32(entry.RuntimeID) == item.Stack.NetworkID {
						name = entry.Name
					}
				}
				if name != "minecraft:pig_spawn_egg" || item.Stack.Count != 1 {
					return fmt.Errorf("wrong picked actor item: %s count=%d", name, item.Stack.Count)
				}
				for _, other := range v.Content[1:] {
					if other.Stack.Count != 0 {
						return fmt.Errorf("repeated actor pick duplicated the egg")
					}
				}
				received = true
			}
		}
		if !received {
			return fmt.Errorf("actor pick exceeded packet budget")
		}
	}
	fmt.Println("PASS: native actor pick creates and reuses the correct egg; range and 64-bit ID alias rejected")
	return nil
}
