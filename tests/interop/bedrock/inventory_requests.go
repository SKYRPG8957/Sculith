package main

import (
	"fmt"
	"time"

	"github.com/sandertv/gophertunnel/minecraft"
	"github.com/sandertv/gophertunnel/minecraft/protocol"
	"github.com/sandertv/gophertunnel/minecraft/protocol/packet"
)

type inventoryProbe struct {
	c             *minecraft.Conn
	items         [36]protocol.ItemInstance
	expected      [36]int32
	id            int32
	cursorID      int32
	reply         protocol.ItemStackResponse
	blocks        map[protocol.BlockPos]uint32
	storageWindow uint32
	storage       []protocol.ItemInstance
}

func (p *inventoryProbe) observe(pk packet.Packet) {
	switch v := pk.(type) {
	case *packet.UpdateBlock:
		if p.blocks != nil {
			p.blocks[v.Position] = v.NewBlockRuntimeID
		}
	case *packet.InventoryContent:
		if p.storage != nil && v.WindowID == p.storageWindow && len(v.Content) == len(p.storage) {
			copy(p.storage, v.Content)
		}
		if v.WindowID == 0 && len(v.Content) == len(p.items) {
			copy(p.items[:], v.Content)
		}
	case *packet.InventorySlot:
		if p.storage != nil && v.WindowID == p.storageWindow && v.Slot < uint32(len(p.storage)) {
			p.storage[v.Slot] = v.NewItem
		}
		if v.WindowID == 0 && v.Slot < uint32(len(p.items)) {
			p.items[v.Slot] = v.NewItem
		}
	}
}

func (p *inventoryProbe) seed() error {
	ids := map[string]int32{}
	for _, item := range p.c.GameData().Items {
		ids[item.Name] = int32(item.RuntimeID)
	}
	p.c.SetReadDeadline(time.Now().Add(8 * time.Second))
	for n := 0; n < 8192; n++ {
		pk, err := p.c.ReadPacket()
		if err != nil {
			return fmt.Errorf("waiting for console seed: %w", err)
		}
		p.observe(pk)
		if p.items[0].Stack.Count == 64 && p.items[1].Stack.Count == 64 && p.items[2].Stack.Count == 64 {
			if p.items[0].Stack.NetworkID != ids["minecraft:diamond_block"] || p.items[1].Stack.NetworkID != ids["minecraft:diamond_block"] || p.items[2].Stack.NetworkID != ids["minecraft:grass_block"] {
				return fmt.Errorf("incorrect seed items")
			}
			for slot := range p.expected {
				p.expected[slot] = p.items[0].Stack.NetworkID
			}
			p.expected[2] = p.items[2].Stack.NetworkID
			return nil
		}
	}
	return fmt.Errorf("missing seed stacks")
}

func (p *inventoryProbe) slot(index byte) protocol.StackRequestSlotInfo {
	return protocol.StackRequestSlotInfo{
		Container: protocol.FullContainerName{ContainerID: protocol.ContainerCombinedHotBarAndInventory},
		Slot:      index, StackNetworkID: p.items[index].StackNetworkID,
	}
}

func (p *inventoryProbe) run(actions []protocol.StackRequestAction, accepted bool, counts map[int]uint16, cursorCount uint16, drop bool) error {
	p.id--
	if err := p.c.WritePacket(&packet.ItemStackRequest{Requests: []protocol.ItemStackRequest{{RequestID: p.id, Actions: actions}}}); err != nil {
		return err
	}
	if err := p.c.Flush(); err != nil {
		return err
	}
	return p.response(accepted, counts, cursorCount, drop)
}

func (p *inventoryProbe) response(accepted bool, counts map[int]uint16, cursorCount uint16, drop bool) error {
	p.c.SetReadDeadline(time.Now().Add(6 * time.Second))
	responded, main, cursor, dropped := false, false, false, !drop
	for n := 0; n < 8192 && !(responded && main && cursor && dropped); n++ {
		pk, err := p.c.ReadPacket()
		if err != nil {
			return fmt.Errorf("request %d: %w", p.id, err)
		}
		p.observe(pk)
		switch v := pk.(type) {
		case *packet.ItemStackResponse:
			for _, response := range v.Responses {
				if response.RequestID == p.id {
					if (response.Status == protocol.ItemStackResponseStatusOK) != accepted {
						return fmt.Errorf("request %d status=%d accepted=%t", p.id, response.Status, accepted)
					}
					responded = true
					p.reply = response
				}
			}
		case *packet.InventoryContent:
			if responded && v.WindowID == 0 && len(v.Content) == len(p.items) {
				for slot, item := range p.items {
					if item.Stack.Count != counts[slot] || (counts[slot] > 0 && item.Stack.NetworkID != p.expected[slot]) {
						return fmt.Errorf("request %d slot%d count=%d expected=%d", p.id, slot, item.Stack.Count, counts[slot])
					}
				}
				main = true
			}
		case *packet.InventorySlot:
			if responded && v.WindowID == 124 && v.Slot == 0 {
				if v.NewItem.Stack.Count != cursorCount || (cursorCount > 0 && v.NewItem.Stack.NetworkID != p.expected[3]) {
					return fmt.Errorf("request %d cursor=%d expected=%d", p.id, v.NewItem.Stack.Count, cursorCount)
				}
				cursor = true
				p.cursorID = v.NewItem.StackNetworkID
			}
		case *packet.AddItemActor:
			if !drop {
				return fmt.Errorf("request %d spawned an unexpected drop", p.id)
			}
			dropped = v.Item.Stack.Count == 1 && v.Item.Stack.NetworkID == p.expected[3]
		}
	}
	if !(responded && main && cursor && dropped) {
		return fmt.Errorf("request %d missing correction or drop", p.id)
	}
	return nil
}

func (p *inventoryProbe) noDrop() error {
	// Give deferred actor tracking time to publish an incorrectly spawned drop.
	time.Sleep(150 * time.Millisecond)
	const barrier = 0x626f756e6400
	if err := p.c.WritePacket(&packet.NetworkStackLatency{Timestamp: barrier}); err != nil {
		return err
	}
	if err := p.c.Flush(); err != nil {
		return err
	}
	p.c.SetReadDeadline(time.Now().Add(4 * time.Second))
	for n := 0; n < 8192; n++ {
		pk, err := p.c.ReadPacket()
		if err != nil {
			return err
		}
		p.observe(pk)
		if _, ok := pk.(*packet.AddItemActor); ok {
			return fmt.Errorf("rejected request spawned a deferred drop")
		}
		if latency, ok := pk.(*packet.NetworkStackLatency); ok && latency.Timestamp == barrier && !latency.NeedsResponse {
			return nil
		}
	}
	return fmt.Errorf("missing inventory drop barrier")
}

func checkInventoryRequests(c *minecraft.Conn, spectator bool) error {
	p := inventoryProbe{c: c}
	if err := p.seed(); err != nil {
		return err
	}
	counts := map[int]uint16{0: 64, 1: 64, 2: 64}
	take := func(count byte, from, to protocol.StackRequestSlotInfo) protocol.StackRequestAction {
		action := &protocol.TakeStackRequestAction{}
		action.Count, action.Source, action.Destination = count, from, to
		return action
	}
	if spectator {
		if err := p.run([]protocol.StackRequestAction{take(1, p.slot(0), p.slot(8))}, false, counts, 0, false); err != nil {
			return err
		}
		fmt.Println("PASS: spectator cannot move inventory stacks")
		return nil
	}
	bad := p.slot(3)
	bad.Slot = 255
	badCursor := protocol.StackRequestSlotInfo{Container: protocol.FullContainerName{ContainerID: protocol.ContainerCursor}, Slot: 1}
	badOffhand := protocol.StackRequestSlotInfo{Container: protocol.FullContainerName{ContainerID: protocol.ContainerOffhand}, Slot: 2}
	consume := &protocol.ConsumeStackRequestAction{}
	consume.Count, consume.Source = 1, p.slot(1)
	invalid := []protocol.StackRequestAction{
		take(1, p.slot(1), p.slot(2)), // Incompatible grass stack.
		take(1, p.slot(1), p.slot(8)), // Prefix swap puts a full stack in slot8.
		take(255, p.slot(1), p.slot(3)),
		take(0, p.slot(1), p.slot(3)),
		take(1, p.slot(1), bad),
		take(1, p.slot(1), badCursor),
		take(1, p.slot(1), badOffhand),
		consume,
	}
	if c.GameData().PlayerGameMode != packet.GameTypeCreative {
		invalid = append(invalid, &protocol.DestroyStackRequestAction{Count: 1, Source: p.slot(1)})
	}
	for n, last := range invalid {
		actions := []protocol.StackRequestAction{&protocol.SwapStackRequestAction{Source: p.slot(0), Destination: p.slot(8)}, last}
		if err := p.run(actions, false, counts, 0, false); err != nil {
			return fmt.Errorf("invalid case%d: %w", n, err)
		}
	}
	if err := p.run([]protocol.StackRequestAction{&protocol.DropStackRequestAction{Count: 1, Source: p.slot(0)}, take(1, p.slot(1), bad)}, false, counts, 0, false); err != nil {
		return err
	}
	if err := p.noDrop(); err != nil {
		return err
	}
	if err := p.run([]protocol.StackRequestAction{take(1, p.slot(0), p.slot(0))}, true, counts, 0, false); err != nil {
		return err
	}
	cursor := protocol.StackRequestSlotInfo{Container: protocol.FullContainerName{ContainerID: protocol.ContainerCursor}}
	counts[0] = 48
	if err := p.run([]protocol.StackRequestAction{take(16, p.slot(0), cursor)}, true, counts, 16, false); err != nil {
		return err
	}
	counts[3] = 16
	cursor.StackNetworkID = p.cursorID
	if err := p.run([]protocol.StackRequestAction{take(16, cursor, p.slot(3))}, true, counts, 0, false); err != nil {
		return err
	}
	counts[3] = 15
	if err := p.run([]protocol.StackRequestAction{&protocol.DropStackRequestAction{Count: 1, Source: p.slot(3)}}, true, counts, 0, true); err != nil {
		return err
	}
	counts[3], counts[9] = 0, 15
	if err := p.run([]protocol.StackRequestAction{&protocol.SwapStackRequestAction{Source: p.slot(3), Destination: p.slot(9)}}, true, counts, 0, false); err != nil {
		return err
	}
	if c.GameData().PlayerGameMode == packet.GameTypeCreative {
		counts[9] = 14
		if err := p.run([]protocol.StackRequestAction{&protocol.DestroyStackRequestAction{Count: 1, Source: p.slot(9)}}, true, counts, 0, false); err != nil {
			return err
		}
	}
	fmt.Println("PASS: ordinary requests reject bounds without partial swaps or world drops; same-slot, cursor split and drop retain state")
	return nil
}

func checkInventoryEvents(c *minecraft.Conn, mutate, drop bool) error {
	p := inventoryProbe{c: c}
	if err := p.seed(); err != nil {
		return err
	}
	actions := []protocol.StackRequestAction{&protocol.SwapStackRequestAction{Source: p.slot(0), Destination: p.slot(8)}}
	if drop {
		actions = append(actions, &protocol.DropStackRequestAction{Count: 64, Source: p.slot(8)})
	}
	counts := map[int]uint16{0: 64, 1: 64, 2: 64}
	if mutate {
		counts[0] = 7
		p.expected[0] = p.expected[2]
	}
	if err := p.run(actions, false, counts, 0, false); err != nil {
		return err
	}
	if mutate && p.items[0].Stack.NetworkID != p.items[2].Stack.NetworkID {
		return fmt.Errorf("callback's grass stack was overwritten")
	}
	if drop {
		if err := p.noDrop(); err != nil {
			return err
		}
	}
	fmt.Println("PASS: ordinary inventory callbacks reject the whole plan and retain callback state")
	return nil
}

func checkClickCursor(c *minecraft.Conn) error {
	p := inventoryProbe{c: c}
	if err := p.seed(); err != nil {
		return err
	}
	counts := map[int]uint16{0: 63, 1: 64, 2: 64}
	cursor := protocol.StackRequestSlotInfo{Container: protocol.FullContainerName{ContainerID: protocol.ContainerCursor}}
	take := &protocol.TakeStackRequestAction{}
	take.Count, take.Source, take.Destination = 1, p.slot(0), cursor
	if err := p.run([]protocol.StackRequestAction{take}, true, counts, 1, false); err != nil {
		return err
	}
	cursor.StackNetworkID = p.cursorID
	if err := p.run([]protocol.StackRequestAction{&protocol.DropStackRequestAction{Count: 1, Source: cursor}}, false, counts, 1, false); err != nil {
		return err
	}
	if err := p.noDrop(); err != nil {
		return err
	}
	place := &protocol.PlaceStackRequestAction{}
	cursor.StackNetworkID = p.cursorID
	place.Count, place.Source, place.Destination = 1, cursor, p.slot(0)
	counts[0] = 64
	if err := p.run([]protocol.StackRequestAction{place}, true, counts, 0, false); err != nil {
		return err
	}
	fmt.Println("PASS: outside click veto retains cursor; destination click returns the item without a world drop")
	return nil
}

func checkCreativeDestroy(c *minecraft.Conn, option string) error {
	if c.GameData().PlayerGameMode != packet.GameTypeCreative {
		return fmt.Errorf("creative destruction requires Creative mode")
	}
	p := inventoryProbe{c: c}
	if err := p.seed(); err != nil {
		return err
	}
	counts := map[int]uint16{0: 64, 1: 64, 2: 64}
	if option == "--destroy-mutate" {
		counts[0], p.expected[0] = 7, p.expected[2]
		if err := p.run([]protocol.StackRequestAction{&protocol.DestroyStackRequestAction{Count: 64, Source: p.slot(0)}}, false, counts, 0, false); err != nil {
			return err
		}
		return p.noDrop()
	}
	allowed := option == "--destroy-allow"
	if !allowed {
		source := p.slot(8)
		source.StackNetworkID = p.id - 1
		if err := p.run([]protocol.StackRequestAction{
			&protocol.SwapStackRequestAction{Source: p.slot(0), Destination: p.slot(8)},
			&protocol.DestroyStackRequestAction{Count: 1, Source: source},
		}, false, counts, 0, false); err != nil {
			return err
		}
	}
	if allowed {
		counts[0] = 61
		next := p.slot(0)
		next.StackNetworkID = p.id - 1
		if err := p.run([]protocol.StackRequestAction{
			&protocol.DestroyStackRequestAction{Count: 1, Source: p.slot(0)},
			&protocol.DestroyStackRequestAction{Count: 2, Source: next},
		}, true, counts, 0, false); err != nil {
			return err
		}
	}
	count := byte(64)
	if allowed {
		count, counts[0] = 61, 0
	}
	if err := p.run([]protocol.StackRequestAction{&protocol.DestroyStackRequestAction{Count: count, Source: p.slot(0)}}, allowed, counts, 0, false); err != nil {
		return err
	}
	from := byte(0)
	if allowed {
		from = 1
	}
	cursor := protocol.StackRequestSlotInfo{Container: protocol.FullContainerName{ContainerID: protocol.ContainerCursor}}
	take := &protocol.TakeStackRequestAction{}
	take.Count, take.Source, take.Destination = 1, p.slot(from), cursor
	counts[int(from)] = 63
	if err := p.run([]protocol.StackRequestAction{take}, true, counts, 1, false); err != nil {
		return err
	}
	cursor.StackNetworkID = p.cursorID
	cursorCount := uint16(1)
	if allowed {
		cursorCount = 0
	}
	if err := p.run([]protocol.StackRequestAction{&protocol.DestroyStackRequestAction{Count: 1, Source: cursor}}, allowed, counts, cursorCount, false); err != nil {
		return err
	}
	if !allowed {
		place := &protocol.PlaceStackRequestAction{}
		cursor.StackNetworkID = p.cursorID
		place.Count, place.Source, place.Destination = 1, cursor, p.slot(from)
		counts[int(from)] = 64
		if err := p.run([]protocol.StackRequestAction{place}, true, counts, 0, false); err != nil {
			return err
		}
	}
	if err := p.noDrop(); err != nil {
		return err
	}
	fmt.Println("PASS: creative destruction honors slot and cursor events without partial moves or world drops")
	return nil
}
