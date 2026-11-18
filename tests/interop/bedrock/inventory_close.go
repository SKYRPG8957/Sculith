package main

import (
	"fmt"
	"time"

	"github.com/sandertv/gophertunnel/minecraft"
	"github.com/sandertv/gophertunnel/minecraft/protocol"
	"github.com/sandertv/gophertunnel/minecraft/protocol/packet"
)

func checkInventoryClose(c *minecraft.Conn, mutate bool) error {
	p := inventoryProbe{c: c}
	if err := p.seed(); err != nil {
		return err
	}
	if err := c.WritePacket(&packet.Interact{ActionType: packet.InteractActionOpenInventory}); err != nil {
		return err
	}
	if err := c.Flush(); err != nil {
		return err
	}
	c.SetReadDeadline(time.Now().Add(6 * time.Second))
	opened := false
	for n := 0; n < 8192 && !opened; n++ {
		pk, err := c.ReadPacket()
		if err != nil {
			return err
		}
		p.observe(pk)
		if open, ok := pk.(*packet.ContainerOpen); ok {
			opened = open.WindowID == 0 && open.ContainerType == 0xff
		}
	}
	if !opened {
		return fmt.Errorf("missing player inventory open response")
	}
	counts := map[int]uint16{0: 63, 1: 64, 2: 64}
	cursor := protocol.StackRequestSlotInfo{Container: protocol.FullContainerName{ContainerID: protocol.ContainerCursor}}
	take := &protocol.TakeStackRequestAction{}
	take.Count, take.Source, take.Destination = 1, p.slot(0), cursor
	if err := p.run([]protocol.StackRequestAction{take}, true, counts, 1, false); err != nil {
		return err
	}
	counts[0] = 64
	cursorCount := uint16(0)
	if mutate {
		counts[0], p.expected[0], p.expected[3], cursorCount = 7, p.expected[2], p.expected[2], 3
	}
	if err := c.WritePacket(&packet.ContainerClose{WindowID: 0, ContainerType: 0xff}); err != nil {
		return err
	}
	if err := c.Flush(); err != nil {
		return err
	}
	closed, inventory, corrected := false, false, false
	checkCursor := func(stack protocol.ItemInstance) error {
		if stack.Stack.Count != cursorCount || (cursorCount > 0 && stack.Stack.NetworkID != p.expected[3]) {
			return fmt.Errorf("close cursor=%d expected=%d", stack.Stack.Count, cursorCount)
		}
		p.cursorID = stack.StackNetworkID
		corrected = true
		return nil
	}
	c.SetReadDeadline(time.Now().Add(6 * time.Second))
	for n := 0; n < 8192 && !(closed && inventory && corrected); n++ {
		pk, err := c.ReadPacket()
		if err != nil {
			return fmt.Errorf("waiting for close correction: %w", err)
		}
		p.observe(pk)
		switch v := pk.(type) {
		case *packet.ContainerClose:
			if v.WindowID != 0 || v.ContainerType != 0xff || v.ServerSide {
				return fmt.Errorf("unexpected close response: %#v", v)
			}
			closed = true
		case *packet.InventoryContent:
			if !closed {
				continue
			}
			if v.Container.ContainerID == protocol.ContainerCursor {
				if v.WindowID != protocol.WindowIDUI || len(v.Content) == 0 {
					return fmt.Errorf("cursor correction window=%d expected=%d", v.WindowID, protocol.WindowIDUI)
				}
				if err := checkCursor(v.Content[0]); err != nil {
					return err
				}
			}
			if v.WindowID == protocol.WindowIDInventory && len(v.Content) == len(p.items) {
				for slot, item := range p.items {
					if item.Stack.Count != counts[slot] || (counts[slot] > 0 && item.Stack.NetworkID != p.expected[slot]) {
						return fmt.Errorf("close slot%d count=%d expected=%d", slot, item.Stack.Count, counts[slot])
					}
				}
				inventory = true
			}
		case *packet.InventorySlot:
			if closed && v.WindowID == protocol.WindowIDUI && v.Slot == 0 {
				if err := checkCursor(v.NewItem); err != nil {
					return err
				}
			}
		case *packet.AddItemActor:
			return fmt.Errorf("closing inventory with room spawned a world drop")
		}
	}
	if !(closed && inventory && corrected) {
		return fmt.Errorf("missing authoritative close correction")
	}
	if err := p.noDrop(); err != nil {
		return err
	}
	if mutate {
		place := &protocol.PlaceStackRequestAction{}
		cursor.StackNetworkID = p.cursorID
		place.Count, place.Source, place.Destination = 3, cursor, p.slot(0)
		counts[0] = 10
		if err := p.run([]protocol.StackRequestAction{place}, true, counts, 0, false); err != nil {
			return err
		}
	}
	fmt.Println("PASS: closing returns held items and corrects the actual cursor through the UI window")
	return nil
}
