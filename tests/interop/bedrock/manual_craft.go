package main

import (
	"fmt"
	"time"

	"github.com/go-gl/mathgl/mgl32"
	"github.com/sandertv/gophertunnel/minecraft"
	"github.com/sandertv/gophertunnel/minecraft/protocol"
	"github.com/sandertv/gophertunnel/minecraft/protocol/packet"
)

func checkManualCraft(c *minecraft.Conn, mode string) error {
	p := inventoryProbe{c: c}
	recipeName, ingredient, output := "minecraft:oak_planks", "minecraft:oak_log", "minecraft:oak_planks"
	grid := []byte{28}
	seed := uint16(2)
	if mode == "--craft-table" {
		recipeName, ingredient, output = "minecraft:Chest_recipeId", "minecraft:oak_planks", "minecraft:chest"
		grid, seed = []byte{32, 33, 34, 35, 37, 38, 39, 40}, 9
	}
	items := make(map[string]int32)
	for _, item := range c.GameData().Items {
		items[item.Name] = int32(item.RuntimeID)
	}
	var recipe protocol.ShapedRecipe
	c.SetReadDeadline(time.Now().Add(8 * time.Second))
	for recipe.RecipeNetworkID == 0 || p.items[0].Stack.Count != seed {
		pk, err := c.ReadPacket()
		if err != nil {
			return fmt.Errorf("waiting for %s and its ingredients: %w", recipeName, err)
		}
		p.observe(pk)
		if data, ok := pk.(*packet.CraftingData); ok {
			for _, entry := range data.ShapedRecipes {
				if entry.RecipeID == recipeName && entry.Block == "crafting_table" && len(entry.Output) == 1 {
					recipe = entry
				}
			}
		}
	}
	if p.items[0].Stack.NetworkID != items[ingredient] || recipe.Output[0].NetworkID != items[output] {
		return fmt.Errorf("unexpected recipe output or seed item")
	}
	if mode == "--craft-table" {
		if err := openCraftingTable(&p); err != nil {
			return err
		}
	} else if err := p.openPlayerInventory(); err != nil {
		return err
	}
	requests := make([]protocol.StackRequestAction, 0, len(grid))
	input := make([]protocol.StackRequestSlotInfo, len(grid))
	for i, slot := range grid {
		input[i] = protocol.StackRequestSlotInfo{Container: protocol.FullContainerName{ContainerID: protocol.ContainerCraftingInput}, Slot: slot}
		move := &protocol.PlaceStackRequestAction{}
		move.Count, move.Source, move.Destination = 1, p.slot(0), input[i]
		requests = append(requests, move)
	}
	if err := c.WritePacket(&packet.ItemStackRequest{Requests: []protocol.ItemStackRequest{{RequestID: -1, Actions: requests}}}); err != nil {
		return err
	}
	if err := c.Flush(); err != nil {
		return err
	}
	c.SetReadDeadline(time.Now().Add(6 * time.Second))
	for input[len(input)-1].StackNetworkID == 0 {
		pk, err := c.ReadPacket()
		if err != nil {
			return fmt.Errorf("waiting for grid fill: %w", err)
		}
		p.observe(pk)
		if reply, ok := pk.(*packet.ItemStackResponse); ok {
			for _, response := range reply.Responses {
				if response.RequestID != -1 {
					continue
				}
				if response.Status != protocol.ItemStackResponseStatusOK {
					return fmt.Errorf("filling the crafting grid failed: %d", response.Status)
				}
				for _, container := range response.ContainerInfo {
					if container.Container.ContainerID == protocol.ContainerCraftingInput {
						for _, slot := range container.SlotInfo {
							for i := range input {
								if input[i].Slot == slot.Slot && slot.Count == 1 {
									input[i].StackNetworkID = slot.StackNetworkID
								}
							}
						}
					}
				}
			}
		}
	}
	actions := []protocol.StackRequestAction{&protocol.CraftRecipeStackRequestAction{RecipeNetworkID: recipe.RecipeNetworkID, NumberOfCrafts: 1}}
	for _, slot := range input {
		if slot.StackNetworkID == 0 {
			return fmt.Errorf("grid slot %d was not filled", slot.Slot)
		}
		consume := &protocol.ConsumeStackRequestAction{}
		consume.Count, consume.Source = 1, slot
		actions = append(actions, consume)
	}
	take := &protocol.TakeStackRequestAction{}
	take.Count, take.Destination = byte(recipe.Output[0].Count), p.slot(1)
	take.Source = protocol.StackRequestSlotInfo{Container: protocol.FullContainerName{ContainerID: protocol.ContainerCreatedOutput}, Slot: 50, StackNetworkID: -3}
	actions = append(actions, take)
	p.id = -2
	accepted := mode == "--craft-allow" || mode == "--craft-table"
	p.expected[0], p.expected[1] = items[ingredient], items[output]
	counts := map[int]uint16{0: 1}
	var gridCount uint16
	if accepted {
		counts[1] = recipe.Output[0].Count
	} else {
		gridCount = 1
	}
	if mode == "--craft-edit" {
		counts[1], p.expected[1] = 7, items["minecraft:grass_block"]
	}
	if err := p.run(actions, accepted, counts, 0, false); err != nil {
		return err
	}
	remaining := make(map[uint32]bool, len(grid))
	for _, slot := range grid {
		remaining[uint32(slot)] = true
	}
	for len(remaining) > 0 {
		pk, err := c.ReadPacket()
		if err != nil {
			return fmt.Errorf("waiting for actual crafting grid: %w", err)
		}
		if slot, ok := pk.(*packet.InventorySlot); ok && slot.WindowID == 124 && remaining[slot.Slot] {
			name, present := slot.Container.Value()
			if !present || name.ContainerID != protocol.ContainerCraftingInput {
				return fmt.Errorf("missing crafting input container for slot %d", slot.Slot)
			}
			if slot.NewItem.Stack.Count != gridCount || (gridCount != 0 && slot.NewItem.Stack.NetworkID != items[ingredient]) {
				return fmt.Errorf("unexpected crafting grid input after request: %+v", slot.NewItem)
			}
			delete(remaining, slot.Slot)
		}
	}
	fmt.Println("PASS: manual craft result and ingredient correction", mode)
	return nil
}

func openCraftingTable(p *inventoryProbe) error {
	transaction := &protocol.UseItemTransactionData{
		ActionType: protocol.UseItemActionClickBlock, BlockPosition: protocol.BlockPos{3, -60, 0}, BlockFace: 1,
		HotBarSlot: 0, HeldItem: p.items[0], Position: p.c.GameData().PlayerPosition, ClickedPosition: mgl32.Vec3{.5, 1, .5},
	}
	if err := p.c.WritePacket(&packet.InventoryTransaction{TransactionData: transaction}); err != nil {
		return err
	}
	if err := p.c.Flush(); err != nil {
		return err
	}
	p.c.SetReadDeadline(time.Now().Add(6 * time.Second))
	for {
		pk, err := p.c.ReadPacket()
		if err != nil {
			return fmt.Errorf("opening the crafting table: %w", err)
		}
		p.observe(pk)
		if open, ok := pk.(*packet.ContainerOpen); ok && open.ContainerType == 1 && open.WindowID != 0 {
			return nil
		}
	}
}
