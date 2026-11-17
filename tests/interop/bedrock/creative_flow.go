package main

import (
	"fmt"
	"time"

	"github.com/sandertv/gophertunnel/minecraft"
	"github.com/sandertv/gophertunnel/minecraft/protocol"
	"github.com/sandertv/gophertunnel/minecraft/protocol/packet"
)

func checkCreativeFlow(c *minecraft.Conn, registry *creativeRegistry) error {
	entry, err := registry.item(c, "minecraft:diamond_block")
	if err != nil {
		return err
	}
	inventory := protocol.FullContainerName{ContainerID: protocol.ContainerCombinedHotBarAndInventory}
	cursor := protocol.FullContainerName{ContainerID: protocol.ContainerCursor}
	var cursorID int32
	run := func(id int32, actions []protocol.StackRequestAction, contents map[int]uint16, cursorCount byte, actualCursor, drop bool) error {
		if err := c.WritePacket(&packet.ItemStackRequest{Requests: []protocol.ItemStackRequest{{RequestID: id, Actions: actions}}}); err != nil {
			return err
		}
		if err := c.Flush(); err != nil {
			return err
		}
		c.SetReadDeadline(time.Now().Add(6 * time.Second))
		responded, main, cursorSeen, dropped := false, false, false, !drop
		for n := 0; n < 4096 && !(responded && main && cursorSeen && dropped); n++ {
			pk, err := c.ReadPacket()
			if err != nil {
				return fmt.Errorf("creative flow %d: %w", id, err)
			}
			switch v := pk.(type) {
			case *packet.ItemStackResponse:
				for _, res := range v.Responses {
					if res.RequestID == id {
						if res.Status != protocol.ItemStackResponseStatusOK {
							return fmt.Errorf("creative flow %d rejected: %d", id, res.Status)
						}
						responded = true
						if !actualCursor {
							for _, container := range res.ContainerInfo {
								if container.Container.ContainerID == protocol.ContainerCursor {
									for _, slot := range container.SlotInfo {
										if slot.Slot == 0 && slot.Count == cursorCount {
											cursorSeen = true
											cursorID = slot.StackNetworkID
										}
									}
								}
							}
						}
					}
				}
			case *packet.InventoryContent:
				if !responded || v.WindowID != 0 || len(v.Content) != 36 {
					continue
				}
				for slot, item := range v.Content {
					if item.Stack.Count != contents[slot] || (contents[slot] > 0 && item.Stack.NetworkID != entry.Item.NetworkID) {
						return fmt.Errorf("creative flow %d slot%d count%d expected%d", id, slot, item.Stack.Count, contents[slot])
					}
				}
				main = true
			case *packet.InventorySlot:
				if responded && v.WindowID == 124 && v.Slot == 0 {
					if v.NewItem.Stack.Count != uint16(cursorCount) || (cursorCount > 0 && v.NewItem.Stack.NetworkID != entry.Item.NetworkID) {
						return fmt.Errorf("creative flow cursor count=%d expected=%d", v.NewItem.Stack.Count, cursorCount)
					}
					cursorSeen = true
					cursorID = v.NewItem.StackNetworkID
				}
			case *packet.AddItemActor:
				if drop && v.Item.Stack.NetworkID == entry.Item.NetworkID && v.Item.Stack.Count == 64 {
					dropped = true
				}
			}
		}
		if !(responded && main && cursorSeen && dropped) {
			return fmt.Errorf("creative flow %d did not finish", id)
		}
		return nil
	}
	creation := func(id int32, to protocol.FullContainerName, slot byte) []protocol.StackRequestAction {
		take := &protocol.TakeStackRequestAction{}
		take.Count = 64
		take.Source = protocol.StackRequestSlotInfo{Container: protocol.FullContainerName{ContainerID: protocol.ContainerCreatedOutput}, Slot: 50, StackNetworkID: id}
		take.Destination = protocol.StackRequestSlotInfo{Container: to, Slot: slot}
		return []protocol.StackRequestAction{&protocol.CraftCreativeStackRequestAction{CreativeItemNetworkID: entry.CreativeItemNetworkID, NumberOfCrafts: 1}, take}
	}
	if err := run(-1, creation(-1, cursor, 0), nil, 64, true, false); err != nil {
		return err
	}
	place := &protocol.PlaceStackRequestAction{}
	place.Count = 32
	place.Source = protocol.StackRequestSlotInfo{Container: cursor, Slot: 0, StackNetworkID: cursorID}
	place.Destination = protocol.StackRequestSlotInfo{Container: inventory, Slot: 0}
	if err := run(-2, []protocol.StackRequestAction{place}, map[int]uint16{0: 32}, 32, false, false); err != nil {
		return err
	}
	if err := run(-3, creation(-3, inventory, 1), map[int]uint16{0: 32, 1: 64}, 32, true, false); err != nil {
		return err
	}
	worldDrop := &protocol.DropStackRequestAction{Count: 64, Source: protocol.StackRequestSlotInfo{Container: protocol.FullContainerName{ContainerID: protocol.ContainerCreatedOutput}, Slot: 50, StackNetworkID: -4}}
	if err := run(-4, []protocol.StackRequestAction{&protocol.CraftCreativeStackRequestAction{CreativeItemNetworkID: entry.CreativeItemNetworkID, NumberOfCrafts: 1}, worldDrop}, map[int]uint16{0: 32, 1: 64}, 32, true, true); err != nil {
		return err
	}
	fmt.Println("PASS: creative cursor, ordinary split, independent creation and world drop retain inventory and cursor state")
	return nil
}
