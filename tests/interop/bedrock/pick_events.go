package main

import (
	"fmt"
	"time"

	"github.com/sandertv/gophertunnel/minecraft"
	"github.com/sandertv/gophertunnel/minecraft/protocol"
	"github.com/sandertv/gophertunnel/minecraft/protocol/packet"
)

func checkPickEvents(c *minecraft.Conn, actors *actorPickRegistry, mutate bool) error {
	write := func(pk packet.Packet) error {
		if err := c.WritePacket(pk); err != nil {
			return err
		}
		return c.Flush()
	}
	const barrier = int64(0x112233445566778)
	if err := write(&packet.NetworkStackLatency{Timestamp: barrier}); err != nil {
		return err
	}
	c.SetReadDeadline(time.Now().Add(6 * time.Second))
	for {
		pk, err := c.ReadPacket()
		if err != nil {
			return err
		}
		if v, ok := pk.(*packet.NetworkStackLatency); ok && v.Timestamp == barrier && !v.NeedsResponse {
			break
		}
	}
	wait := func(selected uint32, count uint16) error {
		c.SetReadDeadline(time.Now().Add(6 * time.Second))
		hotbar := false
		for n := 0; n < 4096; n++ {
			pk, err := c.ReadPacket()
			if err != nil {
				return err
			}
			switch v := pk.(type) {
			case *packet.PlayerHotBar:
				if v.WindowID == 0 && v.SelectHotBarSlot {
					if v.SelectedHotBarSlot != selected {
						return fmt.Errorf("pick event selected %d, expected %d", v.SelectedHotBarSlot, selected)
					}
					hotbar = true
				}
			case *packet.InventoryContent:
				if !hotbar || v.WindowID != 0 || len(v.Content) != 36 {
					continue
				}
				name := ""
				for _, item := range c.GameData().Items {
					if int32(item.RuntimeID) == v.Content[0].Stack.NetworkID {
						name = item.Name
					}
				}
				if name != "minecraft:diamond_block" || v.Content[0].Stack.Count != count {
					return fmt.Errorf("pick event lost inventory: %s count=%d", name, v.Content[0].Stack.Count)
				}
				for slot, item := range v.Content[1:] {
					if item.Stack.Count != 0 {
						return fmt.Errorf("cancelled pick created item in slot %d", slot+1)
					}
				}
				return nil
			}
		}
		return fmt.Errorf("missing pick correction")
	}
	if !mutate {
		if err := write(&packet.BlockPickRequest{Position: protocol.BlockPos{2, -60, 2}, HotBarSlot: 9}); err != nil {
			return err
		}
		if err := wait(0, 1); err != nil {
			return err
		}
	}
	if err := write(&packet.PlayerHotBar{SelectedHotBarSlot: 5, WindowID: 0, SelectHotBarSlot: true}); err != nil {
		return err
	}
	if mutate {
		if err := write(&packet.BlockPickRequest{Position: protocol.BlockPos{0, -61, 2}, HotBarSlot: 9}); err != nil {
			return err
		}
		if err := wait(5, 7); err != nil {
			return err
		}
		fmt.Println("PASS: callback inventory mutation retained; stale pick rejected without holding a guard")
		return nil
	}
	for _, pos := range []protocol.BlockPos{{2, -60, 2}, {0, -61, 2}} {
		if err := write(&packet.BlockPickRequest{Position: pos, HotBarSlot: 9}); err != nil {
			return err
		}
		if err := wait(5, 1); err != nil {
			return err
		}
	}
	actors.Lock()
	near, seen := actors.near, actors.seenNear
	actors.Unlock()
	if !seen {
		return fmt.Errorf("missing actor for cancelled entity pick")
	}
	if err := write(&packet.ActorPickRequest{EntityUniqueID: near, HotBarSlot: 9}); err != nil {
		return err
	}
	if err := wait(5, 1); err != nil {
		return err
	}
	fmt.Println("PASS: Rust plugin cancels hotbar reuse, new block and entity egg picks without inventory changes")
	return nil
}
