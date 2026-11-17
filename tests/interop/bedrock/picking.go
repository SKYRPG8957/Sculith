package main

import (
	"fmt"
	"time"

	"github.com/sandertv/gophertunnel/minecraft"
	"github.com/sandertv/gophertunnel/minecraft/protocol"
	"github.com/sandertv/gophertunnel/minecraft/protocol/packet"
)

func checkPicking(c *minecraft.Conn) error {
	time.Sleep(1200 * time.Millisecond)
	name := func(item protocol.ItemInstance) string {
		if item.Stack.Count == 0 {
			return ""
		}
		for _, entry := range c.GameData().Items {
			if int32(entry.RuntimeID) == item.Stack.NetworkID {
				return entry.Name
			}
		}
		return fmt.Sprintf("unknown:%d", item.Stack.NetworkID)
	}
	write := func(pk packet.Packet) error {
		if err := c.WritePacket(pk); err != nil {
			return err
		}
		return c.Flush()
	}
	var snapshot []protocol.ItemInstance
	pick := func(pos protocol.BlockPos, hint byte, selected uint32, contents map[int]string) error {
		if err := write(&packet.BlockPickRequest{Position: pos, HotBarSlot: hint}); err != nil {
			return err
		}
		c.SetReadDeadline(time.Now().Add(6 * time.Second))
		hotbar := false
		for n := 0; n < 4096; n++ {
			pk, err := c.ReadPacket()
			if err != nil {
				return fmt.Errorf("pick %v hint=%d expected-slot=%d: %w", pos, hint, selected, err)
			}
			switch v := pk.(type) {
			case *packet.PlayerHotBar:
				hotbar = v.WindowID == 0 && v.SelectHotBarSlot && v.SelectedHotBarSlot == selected
			case *packet.InventoryContent:
				if !hotbar || v.WindowID != 0 || len(v.Content) != 36 {
					continue
				}
				matches := true
				for slot, item := range v.Content {
					if name(item) != contents[slot] || (contents[slot] != "" && item.Stack.Count != 1) {
						matches = false
						break
					}
				}
				if matches {
					snapshot = v.Content
					fmt.Printf("PICK hint=%d selected=%d contents=%v\n", hint, selected, contents)
					return nil
				}
			}
		}
		return fmt.Errorf("pick %v exceeded packet budget", pos)
	}
	diamond := protocol.BlockPos{2, -60, 2}
	grass := protocol.BlockPos{0, -61, 2}
	if err := pick(diamond, 9, 0, map[int]string{0: "minecraft:diamond_block"}); err != nil {
		return err
	}
	if err := write(&packet.PlayerHotBar{SelectedHotBarSlot: 5, WindowID: 0, SelectHotBarSlot: true}); err != nil {
		return err
	}
	contents := map[int]string{0: "minecraft:diamond_block", 1: "minecraft:grass_block"}
	if err := pick(grass, 8, 1, contents); err != nil {
		return err
	}
	if err := pick(diamond, 8, 0, contents); err != nil {
		return err
	}
	if err := pick(grass, 0, 1, contents); err != nil {
		return err
	}
	container := protocol.FullContainerName{ContainerID: protocol.ContainerCombinedHotBarAndInventory}
	if err := write(&packet.ItemStackRequest{Requests: []protocol.ItemStackRequest{{
		RequestID: 1,
		Actions: []protocol.StackRequestAction{&protocol.SwapStackRequestAction{
			Source:      protocol.StackRequestSlotInfo{Container: container, Slot: 0, StackNetworkID: snapshot[0].StackNetworkID},
			Destination: protocol.StackRequestSlotInfo{Container: container, Slot: 10, StackNetworkID: snapshot[10].StackNetworkID},
		}},
	}}}); err != nil {
		return err
	}
	c.SetReadDeadline(time.Now().Add(6 * time.Second))
	approved := false
	for n := 0; n < 4096 && !approved; n++ {
		pk, err := c.ReadPacket()
		if err != nil {
			return fmt.Errorf("move picked stack to storage: %w", err)
		}
		if response, ok := pk.(*packet.ItemStackResponse); ok {
			for _, entry := range response.Responses {
				if entry.RequestID == 1 {
					if entry.Status != protocol.ItemStackResponseStatusOK {
						return fmt.Errorf("inventory swap rejected: %d", entry.Status)
					}
					approved = true
				}
			}
		}
	}
	if !approved {
		return fmt.Errorf("missing inventory swap response")
	}
	if err := pick(diamond, 9, 1, map[int]string{1: "minecraft:diamond_block", 10: "minecraft:grass_block"}); err != nil {
		return err
	}
	if err := pick(grass, 0, 1, map[int]string{1: "minecraft:grass_block", 10: "minecraft:diamond_block"}); err != nil {
		return err
	}
	fmt.Println("PASS: native picks select hotbar and storage stacks without losing or duplicating items")
	return nil
}
