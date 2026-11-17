package main

import (
	"fmt"
	"time"

	"github.com/sandertv/gophertunnel/minecraft"
	"github.com/sandertv/gophertunnel/minecraft/protocol"
	"github.com/sandertv/gophertunnel/minecraft/protocol/packet"
)

func checkCreativeEvents(c *minecraft.Conn, registry *creativeRegistry, mutate bool) error {
	entry, err := registry.item(c, "minecraft:diamond_block")
	if err != nil {
		return err
	}
	var grass int32
	diamond := entry.Item.NetworkID
	for _, item := range c.GameData().Items {
		if item.Name == "minecraft:grass_block" {
			grass = int32(item.RuntimeID)
		}
	}
	write := func(pk packet.Packet) error {
		if err := c.WritePacket(pk); err != nil {
			return err
		}
		return c.Flush()
	}
	var original []protocol.ItemInstance
	if !mutate {
		if err := write(&packet.BlockPickRequest{Position: protocol.BlockPos{2, -60, 2}, HotBarSlot: 9}); err != nil {
			return err
		}
		c.SetReadDeadline(time.Now().Add(6 * time.Second))
		for n := 0; n < 4096; n++ {
			pk, err := c.ReadPacket()
			if err != nil {
				return err
			}
			if content, ok := pk.(*packet.InventoryContent); ok && content.WindowID == 0 && len(content.Content) == 36 && content.Content[0].Stack.NetworkID == diamond && content.Content[0].Stack.Count == 1 {
				original = content.Content
				break
			}
		}
		if original == nil {
			return fmt.Errorf("missing seed inventory")
		}
	}
	container := protocol.FullContainerName{ContainerID: protocol.ContainerCombinedHotBarAndInventory}
	for n, kind := range []string{"inventory", "cursor", "drop"} {
		if mutate && n > 0 {
			break
		}
		id := -int32(n + 1)
		actions := []protocol.StackRequestAction{}
		if !mutate && n == 0 {
			actions = append(actions, &protocol.SwapStackRequestAction{
				Source:      protocol.StackRequestSlotInfo{Container: container, Slot: 0, StackNetworkID: original[0].StackNetworkID},
				Destination: protocol.StackRequestSlotInfo{Container: container, Slot: 8},
			})
		}
		actions = append(actions, &protocol.CraftCreativeStackRequestAction{CreativeItemNetworkID: entry.CreativeItemNetworkID, NumberOfCrafts: 1},
			&protocol.CraftResultsDeprecatedStackRequestAction{ResultItems: []protocol.StackRequestItem{{Identifier: "minecraft:diamond_block", BlockRuntimeID: entry.Item.BlockRuntimeID, Count: 64}}, TimesCrafted: 1})
		source := protocol.StackRequestSlotInfo{Container: protocol.FullContainerName{ContainerID: protocol.ContainerCreatedOutput}, Slot: 50, StackNetworkID: id}
		if kind == "drop" {
			drop := &protocol.DropStackRequestAction{}
			drop.Count = 64
			drop.Source = source
			actions = append(actions, drop)
		} else {
			take := &protocol.TakeStackRequestAction{}
			take.Count = 64
			take.Source = source
			take.Destination = protocol.StackRequestSlotInfo{Container: container, Slot: 0}
			if kind == "cursor" {
				take.Destination.Container = protocol.FullContainerName{ContainerID: protocol.ContainerCursor}
			}
			actions = append(actions, take)
		}
		if err := write(&packet.ItemStackRequest{Requests: []protocol.ItemStackRequest{{RequestID: id, Actions: actions}}}); err != nil {
			return err
		}
		c.SetReadDeadline(time.Now().Add(6 * time.Second))
		responded, synced, cursorSynced := false, false, false
		for j := 0; j < 4096 && !(synced && cursorSynced); j++ {
			pk, err := c.ReadPacket()
			if err != nil {
				return err
			}
			switch v := pk.(type) {
			case *packet.AddItemActor:
				return fmt.Errorf("unexpected item before creative correction: item=%d count=%d position=%v kind=%s mutation=%t responded=%t", v.Item.Stack.NetworkID, v.Item.Stack.Count, v.Position, kind, mutate, responded)
			case *packet.ItemStackResponse:
				for _, res := range v.Responses {
					if res.RequestID == id {
						if res.Status == protocol.ItemStackResponseStatusOK {
							return fmt.Errorf("creative %s callback did not reject request", kind)
						}
						responded = true
					}
				}
			case *packet.InventorySlot:
				if responded && v.WindowID == 124 && v.Slot == 0 {
					if v.NewItem.Stack.Count != 0 {
						return fmt.Errorf("cancelled creative request changed the cursor")
					}
					cursorSynced = true
				}
			case *packet.InventoryContent:
				if !responded || v.WindowID != 0 || len(v.Content) != 36 {
					continue
				}
				wantID, wantCount := diamond, uint16(1)
				if mutate {
					wantID, wantCount = grass, 7
				}
				for slot, item := range v.Content {
					if slot == 0 {
						if item.Stack.NetworkID != wantID || item.Stack.Count != wantCount {
							return fmt.Errorf("creative callback state lost: slot0 item=%d count=%d", item.Stack.NetworkID, item.Stack.Count)
						}
					} else if item.Stack.Count != 0 {
						return fmt.Errorf("cancelled creative request changed slot %d", slot)
					}
				}
				synced = true
			}
		}
		if !synced || !cursorSynced {
			return fmt.Errorf("missing creative inventory correction")
		}
		fmt.Printf("CREATIVE_EVENT kind=%s mutation=%t rejected without partial inventory changes\n", kind, mutate)
	}
	fmt.Println("PASS: creative callbacks run before changes, cancel atomically and preserve callback inventory mutations")
	return nil
}
