package main

import (
	"fmt"
	"time"

	"github.com/go-gl/mathgl/mgl32"
	"github.com/sandertv/gophertunnel/minecraft"
	"github.com/sandertv/gophertunnel/minecraft/protocol"
	"github.com/sandertv/gophertunnel/minecraft/protocol/packet"
)

func checkStorageInventory(c *minecraft.Conn, mode string) error {
	p := inventoryProbe{c: c}
	if err := p.seed(); err != nil {
		return err
	}
	var containerType byte
	openStorage := func() error {
		if p.storage != nil {
			if err := c.WritePacket(&packet.ContainerClose{WindowID: byte(p.storageWindow), ContainerType: containerType}); err != nil {
				return err
			}
		}
		p.storage = nil
		if err := c.WritePacket(&packet.InventoryTransaction{TransactionData: &protocol.UseItemTransactionData{
			ActionType: protocol.UseItemActionClickBlock, BlockPosition: protocol.BlockPos{3, -60, 0}, BlockFace: 1,
			HotBarSlot: 0, HeldItem: p.items[0], Position: c.GameData().PlayerPosition, ClickedPosition: mgl32.Vec3{.5, 1, .5},
		}}); err != nil {
			return err
		}
		if err := c.Flush(); err != nil {
			return err
		}
		c.SetReadDeadline(time.Now().Add(8 * time.Second))
		for n := 0; n < 8192; n++ {
			pk, err := c.ReadPacket()
			if err != nil {
				return fmt.Errorf("opening chest: %w", err)
			}
			if open, ok := pk.(*packet.ContainerOpen); ok {
				if open.WindowID == 0 || open.WindowID == 0xff {
					return fmt.Errorf("chest did not open a storage window")
				}
				p.storageWindow, p.storage = uint32(open.WindowID), make([]protocol.ItemInstance, 27)
				containerType = open.ContainerType
			}
			p.observe(pk)
			if content, ok := pk.(*packet.InventoryContent); ok && p.storage != nil && content.WindowID == p.storageWindow && len(content.Content) == 27 {
				return nil
			}
		}
		return fmt.Errorf("missing chest contents")
	}
	if err := openStorage(); err != nil {
		return err
	}
	if p.storage[0].Stack.Count != 64 || p.storage[1].Stack.Count != 64 {
		return fmt.Errorf("incorrect seeded chest contents")
	}
	chest := func(slot byte) protocol.StackRequestSlotInfo {
		return protocol.StackRequestSlotInfo{Container: protocol.FullContainerName{ContainerID: protocol.ContainerLevelEntity},
			Slot: slot, StackNetworkID: p.storage[slot].StackNetworkID}
	}
	take := func(count byte, from, to protocol.StackRequestSlotInfo) protocol.StackRequestAction {
		a := &protocol.TakeStackRequestAction{}
		a.Count, a.Source, a.Destination = count, from, to
		return a
	}
	counts := map[int]uint16{0: 64, 1: 64, 2: 64}
	storage := map[int]uint16{0: 64, 1: 64}
	storageIDs := map[int]int32{0: p.expected[0], 1: p.expected[2]}
	checkContents := func() error {
		for slot, item := range p.storage {
			if item.Stack.Count != storage[slot] || (storage[slot] > 0 && item.Stack.NetworkID != storageIDs[slot]) {
				return fmt.Errorf("storage slot%d item=%d count=%d expected item=%d count=%d", slot, item.Stack.NetworkID, item.Stack.Count, storageIDs[slot], storage[slot])
			}
		}
		return nil
	}
	run := func(actions []protocol.StackRequestAction, accepted bool) error {
		p.id--
		if err := c.WritePacket(&packet.ItemStackRequest{Requests: []protocol.ItemStackRequest{{RequestID: p.id, Actions: actions}}}); err != nil {
			return err
		}
		if err := c.Flush(); err != nil {
			return err
		}
		c.SetReadDeadline(time.Now().Add(6 * time.Second))
		responded := false
		for n := 0; n < 8192 && !responded; n++ {
			pk, err := c.ReadPacket()
			if err != nil {
				return err
			}
			p.observe(pk)
			if reply, ok := pk.(*packet.ItemStackResponse); ok {
				for _, response := range reply.Responses {
					if response.RequestID == p.id {
						if (response.Status == protocol.ItemStackResponseStatusOK) != accepted {
							return fmt.Errorf("storage request %d status=%d expected accepted=%t", p.id, response.Status, accepted)
						}
						responded = true
					}
				}
			}
			if _, ok := pk.(*packet.AddItemActor); ok {
				return fmt.Errorf("storage request spawned an unexpected drop")
			}
		}
		if !responded {
			return fmt.Errorf("missing storage response")
		}
		// Read current server inventory after the response, even if an old error path
		// failed to correct the owner's prediction. Legacy queries never move items.
		p.id -= 2
		if p.id%2 != 0 {
			p.id--
		}
		if err := c.WritePacket(&packet.InventoryTransaction{LegacyRequestID: p.id, TransactionData: &protocol.NormalTransactionData{}}); err != nil {
			return err
		}
		if err := c.Flush(); err != nil {
			return err
		}
		if err := p.response(true, counts, 0, false); err != nil {
			return fmt.Errorf("storage owner readback: %w", err)
		}
		if err := p.noDrop(); err != nil {
			return err
		}
		// Stack responses contain count/network-ID deltas, not full item identities.
		// Reopen to read the actual server container rather than a predicted cache.
		if err := openStorage(); err != nil {
			return err
		}
		return checkContents()
	}
	if mode != "--storage-inventory" {
		actions := []protocol.StackRequestAction{take(1, chest(0), p.slot(8)), take(1, chest(1), p.slot(9))}
		if mode == "--storage-mutate" {
			storage[0], storageIDs[0] = 7, p.expected[2]
		} else if mode == "--storage-drop-deny" {
			actions[1] = &protocol.DropStackRequestAction{Count: 1, Source: chest(1)}
		}
		if err := p.run(actions, false, counts, 0, false); err != nil {
			return err
		}
		if err := p.noDrop(); err != nil {
			return err
		}
		if err := checkContents(); err != nil {
			return fmt.Errorf("storage correction: %w", err)
		}
		if err := openStorage(); err != nil {
			return err
		}
		if err := checkContents(); err != nil {
			return fmt.Errorf("storage readback: %w", err)
		}
		fmt.Println("PASS: storage callback rejected the plan and retained actual inventory")
		return nil
	}
	counts[8], storage[0] = 1, 63
	if err := run([]protocol.StackRequestAction{take(1, chest(0), p.slot(8))}, true); err != nil {
		return fmt.Errorf("withdraw control: %w", err)
	}
	counts[8], storage[0] = 0, 64
	if err := run([]protocol.StackRequestAction{take(1, p.slot(8), chest(0))}, true); err != nil {
		return fmt.Errorf("deposit control: %w", err)
	}
	for _, index := range []byte{27, 255} {
		bad := chest(0)
		bad.Slot = index
		if err := run([]protocol.StackRequestAction{take(1, chest(0), p.slot(8)), take(1, bad, p.slot(9))}, false); err != nil {
			return fmt.Errorf("invalid storage slot%d after withdrawal: %w", index, err)
		}
	}
	bad := chest(0)
	bad.Slot = 27
	if err := run([]protocol.StackRequestAction{
		&protocol.DropStackRequestAction{Count: 1, Source: chest(0)}, take(1, bad, p.slot(9)),
	}, false); err != nil {
		return fmt.Errorf("invalid action after storage drop: %w", err)
	}
	consume := &protocol.ConsumeStackRequestAction{}
	consume.Count, consume.Source = 1, chest(1)
	if err := run([]protocol.StackRequestAction{take(1, chest(0), p.slot(8)), consume}, false); err != nil {
		return fmt.Errorf("consumption without crafting context: %w", err)
	}
	fmt.Println("PASS: chest withdrawals and deposits work; a later invalid action leaves every storage and player slot intact")
	return nil
}
