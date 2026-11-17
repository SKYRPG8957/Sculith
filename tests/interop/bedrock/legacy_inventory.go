package main

import (
	"fmt"
	"time"

	dfworld "github.com/df-mc/dragonfly/server/world"
	"github.com/go-gl/mathgl/mgl32"
	"github.com/sandertv/gophertunnel/minecraft"
	"github.com/sandertv/gophertunnel/minecraft/protocol"
	"github.com/sandertv/gophertunnel/minecraft/protocol/packet"
)

func checkLegacyInventory(c *minecraft.Conn, veto, heldVeto bool) error {
	if c.GameData().PlayerGameMode != packet.GameTypeSurvival {
		return fmt.Errorf("legacy inventory test must join in Survival")
	}
	p := inventoryProbe{c: c, blocks: map[protocol.BlockPos]uint32{}}
	if err := p.seed(); err != nil {
		return err
	}
	counts := map[int]uint16{0: 64, 1: 64, 2: 64}
	container := func(slot uint32, old, new protocol.ItemInstance) protocol.InventoryAction {
		return protocol.InventoryAction{SourceType: protocol.InventoryActionSourceContainer, WindowID: protocol.Option(int8(protocol.WindowIDInventory)), InventorySlot: slot, OldItem: old, NewItem: new}
	}
	world := func(stack protocol.ItemInstance) protocol.InventoryAction {
		return protocol.InventoryAction{SourceType: protocol.InventoryActionSourceWorld, SourceFlags: protocol.Option(uint32(0)), NewItem: stack}
	}
	send := func(data protocol.InventoryTransactionData, actions []protocol.InventoryAction, slots []protocol.LegacySetItemSlot, accepted, drop bool) error {
		p.id -= 2
		if err := c.WritePacket(&packet.InventoryTransaction{LegacyRequestID: p.id, LegacySetItemSlots: slots, Actions: actions, TransactionData: data}); err != nil {
			return err
		}
		if err := c.Flush(); err != nil {
			return err
		}
		if err := p.response(accepted, counts, 0, drop); err != nil {
			return err
		}
		if accepted {
			for _, group := range slots {
				for _, slot := range group.Slots {
					found := false
					for _, container := range p.reply.ContainerInfo {
						for _, current := range container.SlotInfo {
							want := byte(0)
							if group.ContainerID == protocol.ContainerInventory {
								want = byte(counts[int(slot)])
							}
							found = found || (container.Container.ContainerID == group.ContainerID && current.Slot == slot && current.Count == want)
						}
					}
					if !found {
						return fmt.Errorf("missing queried slot in legacy response: container=%d slot=%d", group.ContainerID, slot)
					}
				}
			}
		}
		return nil
	}
	interact := func(data protocol.UseItemTransactionData, embedded, accepted bool) error {
		p.id -= 2
		data.LegacyRequestID = p.id
		var message packet.Packet = &packet.InventoryTransaction{LegacyRequestID: p.id, Actions: data.Actions, TransactionData: &data}
		if embedded {
			message = &packet.PlayerAuthInput{Position: c.GameData().PlayerPosition, InputMode: packet.InputModeMouse, PlayMode: packet.PlayModeScreen,
				InteractionModel: packet.InteractionModelCrosshair, Tick: uint64(-p.id), InputData: protocol.NewInputFlagsFromIDs(packet.InputFlagCount, []int32{packet.InputFlagPerformItemInteraction}),
				ItemInteractionData: protocol.Option(data), CameraOrientation: mgl32.Vec3{0, 0, 1}}
		}
		if err := c.WritePacket(message); err != nil {
			return err
		}
		if err := c.Flush(); err != nil {
			return err
		}
		return p.response(accepted, counts, 0, false)
	}
	use := func(slot int32, held protocol.ItemInstance, actions []protocol.InventoryAction, embedded, accepted bool) error {
		return interact(protocol.UseItemTransactionData{ActionType: protocol.UseItemActionClickAir, HotBarSlot: slot, HeldItem: held, Position: c.GameData().PlayerPosition, Actions: actions}, embedded, accepted)
	}
	dropActions := func() []protocol.InventoryAction {
		remaining, dropped := p.items[0], p.items[0]
		remaining.Stack.Count--
		dropped.Stack.Count = 1
		return []protocol.InventoryAction{container(0, p.items[0], remaining), world(dropped)}
	}
	if heldVeto {
		if err := use(1, p.items[1], nil, false, false); err != nil {
			return err
		}
		fmt.Println("PASS: a transaction cannot bypass the held-slot veto")
		return nil
	}
	if veto {
		if err := send(&protocol.NormalTransactionData{}, dropActions(), nil, false, false); err != nil {
			return err
		}
		return p.noDrop()
	}
	if err := send(&protocol.NormalTransactionData{}, nil, []protocol.LegacySetItemSlot{{ContainerID: protocol.ContainerInventory, Slots: []byte{0}}}, true, false); err != nil {
		return fmt.Errorf("slot sync query: %w", err)
	}
	if err := p.noDrop(); err != nil {
		return err
	}
	if err := send(&protocol.NormalTransactionData{}, nil, []protocol.LegacySetItemSlot{{ContainerID: protocol.ContainerArmor, Slots: []byte{0}}, {ContainerID: protocol.ContainerOffhand, Slots: []byte{1}}}, true, false); err != nil {
		return err
	}
	for _, slots := range [][]byte{{0, 1, 2}, {255}} {
		if err := send(&protocol.NormalTransactionData{}, nil, []protocol.LegacySetItemSlot{{ContainerID: protocol.ContainerInventory, Slots: slots}}, false, false); err != nil {
			return err
		}
	}
	for _, data := range []protocol.InventoryTransactionData{&protocol.NormalTransactionData{}, &protocol.MismatchTransactionData{}} {
		if err := send(data, []protocol.InventoryAction{container(8, protocol.ItemInstance{}, p.items[0])}, nil, false, false); err != nil {
			return err
		}
	}
	if err := send(&protocol.NormalTransactionData{}, []protocol.InventoryAction{world(p.items[0])}, nil, false, false); err != nil {
		return err
	}
	invalid := dropActions()
	invalid[0].OldItem.Stack.Count++
	if err := send(&protocol.NormalTransactionData{}, invalid, nil, false, false); err != nil {
		return err
	}
	invalid = dropActions()
	invalid[1].NewItem = p.items[2]
	invalid[1].NewItem.Stack.Count = 1
	if err := send(&protocol.NormalTransactionData{}, invalid, nil, false, false); err != nil {
		return err
	}
	for _, embedded := range []bool{false, true} {
		if err := use(8, p.items[2], nil, embedded, false); err != nil {
			return fmt.Errorf("unowned held item embedded=%t: %w", embedded, err)
		}
		injected := []protocol.InventoryAction{container(8, protocol.ItemInstance{}, p.items[0]), world(p.items[0])}
		if err := use(0, p.items[0], injected, embedded, true); err != nil {
			return fmt.Errorf("predicted journal embedded=%t: %w", embedded, err)
		}
		if err := p.noDrop(); err != nil {
			return err
		}
	}
	if err := use(255, p.items[0], nil, false, false); err != nil {
		return err
	}
	if err := use(2, p.items[2], nil, false, true); err != nil {
		return err
	}
	actions := dropActions()
	counts[0]--
	if err := send(&protocol.NormalTransactionData{}, actions, nil, true, true); err != nil {
		return err
	}
	registry := dfworld.NewBlockRegistry()
	registry.Finalize()
	for index, embedded := range []bool{false, true} {
		target := protocol.BlockPos{1, -60, 3 + int32(index)}
		held, predicted := p.items[0], p.items[0]
		predicted.Stack.Count--
		counts[0]--
		data := protocol.UseItemTransactionData{ActionType: protocol.UseItemActionClickBlock, HotBarSlot: 0, HeldItem: held,
			Position: c.GameData().PlayerPosition, BlockPosition: protocol.BlockPos{target[0], -61, target[2]}, BlockFace: 1,
			ClickedPosition: mgl32.Vec3{.5, 1, .5}, Actions: []protocol.InventoryAction{container(0, held, predicted)}}
		if err := interact(data, embedded, true); err != nil {
			return fmt.Errorf("Survival placement embedded=%t: %w", embedded, err)
		}
		c.SetReadDeadline(time.Now().Add(4 * time.Second))
		for n := 0; n < 4096; n++ {
			if _, ok := p.blocks[target]; ok {
				break
			}
			pk, err := c.ReadPacket()
			if err != nil {
				return err
			}
			p.observe(pk)
		}
		id, found := p.blocks[target]
		if c.GameData().UseBlockNetworkIDHashes {
			var ok bool
			id, ok = registry.HashToRuntimeID(id)
			found = found && ok
		}
		name, _, ok := registry.RuntimeIDToState(id)
		if !found || !ok || name != "minecraft:diamond_block" {
			return fmt.Errorf("Survival placement has no committed diamond block at %v", target)
		}
	}
	if err := send(&protocol.MismatchTransactionData{}, nil, nil, true, false); err != nil {
		return err
	}
	fmt.Println("PASS: legacy sync is read-only; invalid journals and unowned hands cannot grant or drop items; verified drop and Survival placements debit the server stack once")
	return nil
}
