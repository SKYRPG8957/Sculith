package main

import (
	"fmt"
	"time"

	"github.com/sandertv/gophertunnel/minecraft"
	"github.com/sandertv/gophertunnel/minecraft/protocol"
	"github.com/sandertv/gophertunnel/minecraft/protocol/packet"
)

func checkCursorDrag(c *minecraft.Conn, mode string) error {
	p := inventoryProbe{c: c}
	if err := p.seed(); err != nil {
		return err
	}
	if err := p.openPlayerInventory(); err != nil {
		return err
	}
	cursor := protocol.StackRequestSlotInfo{
		Container: protocol.FullContainerName{ContainerID: protocol.ContainerCursor},
	}
	take := func(count byte) protocol.StackRequestAction {
		action := &protocol.TakeStackRequestAction{}
		action.Count, action.Source, action.Destination = count, p.slot(0), cursor
		return action
	}
	place := func(count, slot byte) protocol.StackRequestAction {
		cursor.StackNetworkID = p.cursorID
		action := &protocol.PlaceStackRequestAction{}
		action.Count, action.Source, action.Destination = count, cursor, p.slot(slot)
		return action
	}
	counts := map[int]uint16{0: 58, 1: 64, 2: 64}
	if err := p.run([]protocol.StackRequestAction{take(6)}, true, counts, 6, false); err != nil {
		return err
	}
	for range 2 {
		if err := c.WritePacket(&packet.ServerboundCursorItemDrag{State: packet.CursorItemDragStart}); err != nil {
			return err
		}
	}
	accepted := mode == "--drag-allow"
	remaining := uint16(6)
	if accepted {
		counts[3], counts[4], remaining = 2, 3, 1
	} else if mode == "--drag-edit" {
		counts[4], p.expected[4] = 7, p.expected[2]
	}
	if err := p.run([]protocol.StackRequestAction{place(2, 3), place(3, 4)}, accepted, counts, remaining, false); err != nil {
		return fmt.Errorf("drag placement: %w", err)
	}
	if err := c.WritePacket(&packet.ServerboundCursorItemDrag{State: packet.CursorItemDragStop}); err != nil {
		return err
	}
	counts[5] = remaining
	if err := p.run([]protocol.StackRequestAction{place(byte(remaining), 5)}, true, counts, 0, false); err != nil {
		return fmt.Errorf("placement after drag stop: %w", err)
	}

	if accepted {
		// The player inventory uses the same handler after reopening.
		if err := c.WritePacket(&packet.ServerboundCursorItemDrag{State: packet.CursorItemDragStart}); err != nil {
			return err
		}
		if err := c.WritePacket(&packet.ContainerClose{WindowID: 0, ContainerType: 0xff}); err != nil {
			return err
		}
		if err := c.Flush(); err != nil {
			return err
		}
		c.SetReadDeadline(time.Now().Add(6 * time.Second))
		for {
			pk, err := c.ReadPacket()
			if err != nil {
				return fmt.Errorf("closing during a drag: %w", err)
			}
			p.observe(pk)
			if closed, ok := pk.(*packet.ContainerClose); ok && closed.WindowID == 0 {
				break
			}
		}
		if err := p.openPlayerInventory(); err != nil {
			return err
		}
		cursor.StackNetworkID = p.cursorID
		counts[0] -= 2
		if err := p.run([]protocol.StackRequestAction{take(2)}, true, counts, 2, false); err != nil {
			return err
		}
		counts[6] = 2
		if err := p.run([]protocol.StackRequestAction{place(2, 6)}, true, counts, 0, false); err != nil {
			return fmt.Errorf("placement after reopening: %w", err)
		}
	}
	fmt.Println("PASS: cursor drag result and authoritative inventory", mode)
	return nil
}
