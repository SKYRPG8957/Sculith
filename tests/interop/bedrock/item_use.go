package main

import (
	"fmt"
	"strings"
	"time"

	"github.com/go-gl/mathgl/mgl32"
	"github.com/sandertv/gophertunnel/minecraft"
	"github.com/sandertv/gophertunnel/minecraft/protocol"
	"github.com/sandertv/gophertunnel/minecraft/protocol/packet"
)

func checkItemUseEvents(c *minecraft.Conn, option string) error {
	if c.GameData().PlayerGameMode != packet.GameTypeSurvival {
		return fmt.Errorf("item-use event check requires Survival")
	}
	food, allowed := strings.HasPrefix(option, "--eat-"), strings.HasSuffix(option, "-allow")
	item := "minecraft:leather_helmet"
	if food {
		item = "minecraft:golden_apple"
	}
	ids := map[string]int32{}
	for _, entry := range c.GameData().Items {
		ids[entry.Name] = int32(entry.RuntimeID)
	}
	p := inventoryProbe{c: c}
	c.SetReadDeadline(time.Now().Add(8 * time.Second))
	for n := 0; ; n++ {
		if n == 8192 {
			return fmt.Errorf("missing item-use console seed")
		}
		pk, err := c.ReadPacket()
		if err != nil {
			return err
		}
		p.observe(pk)
		if p.items[0].Stack.Count == 1 && p.items[0].Stack.NetworkID == ids[item] {
			break
		}
	}
	p.expected[0] = ids[item]
	counts := map[int]uint16{0: 1}
	if strings.HasSuffix(option, "-mutate") {
		counts[0], p.expected[0] = 7, ids["minecraft:grass_block"]
	} else if allowed && !food {
		counts[0] = 0
	}
	p.id -= 2
	data := protocol.UseItemTransactionData{LegacyRequestID: p.id, ActionType: protocol.UseItemActionClickAir,
		HotBarSlot: 0, HeldItem: p.items[0], Position: c.GameData().PlayerPosition}
	var message packet.Packet = &packet.InventoryTransaction{LegacyRequestID: p.id, TransactionData: &data}
	if option == "--use-input-deny" {
		message = &packet.PlayerAuthInput{Position: c.GameData().PlayerPosition, InputMode: packet.InputModeMouse,
			PlayMode: packet.PlayModeScreen, InteractionModel: packet.InteractionModelCrosshair, Tick: 1,
			InputData:           protocol.NewInputFlagsFromIDs(packet.InputFlagCount, []int32{packet.InputFlagPerformItemInteraction}),
			ItemInteractionData: protocol.Option(data), CameraOrientation: mgl32.Vec3{0, 1, 0}}
	}
	if err := c.WritePacket(message); err != nil {
		return err
	}
	if err := c.Flush(); err != nil {
		return err
	}
	if err := p.response(true, counts, 0, false); err != nil {
		return err
	}
	if food {
		// Golden apples are always edible and finish after 32 server ticks.
		time.Sleep(2500 * time.Millisecond)
		if allowed {
			counts[0] = 0
		}
	}
	p.id -= 2
	if err := c.WritePacket(&packet.InventoryTransaction{LegacyRequestID: p.id,
		LegacySetItemSlots: []protocol.LegacySetItemSlot{{ContainerID: protocol.ContainerArmor, Slots: []byte{0}}},
		TransactionData:    &protocol.NormalTransactionData{}}); err != nil {
		return err
	}
	if err := c.Flush(); err != nil {
		return err
	}
	if err := p.response(true, counts, 0, false); err != nil {
		return err
	}
	head := byte(0)
	if allowed && !food {
		head = 1
	}
	found := false
	for _, container := range p.reply.ContainerInfo {
		if container.Container.ContainerID == protocol.ContainerArmor {
			for _, slot := range container.SlotInfo {
				if slot.Slot == 0 {
					if slot.Count != head {
						return fmt.Errorf("head count=%d expected=%d", slot.Count, head)
					}
					found = true
				}
			}
		}
	}
	if !found {
		return fmt.Errorf("missing head inventory query response")
	}
	if err := p.noDrop(); err != nil {
		return err
	}
	fmt.Println("PASS: item use honors interaction callbacks before equipment and timed consumption")
	return nil
}
