package main

import (
	"bytes"
	"fmt"
	"net"
	"sync"
	"time"

	"github.com/sandertv/gophertunnel/minecraft"
	"github.com/sandertv/gophertunnel/minecraft/protocol"
	"github.com/sandertv/gophertunnel/minecraft/protocol/packet"
)

type creativeRegistry struct {
	sync.Mutex
	items []protocol.CreativeItem
	err   error
}

func (r *creativeRegistry) observe(header packet.Header, payload []byte, _, _ net.Addr) {
	if header.PacketID != packet.IDCreativeContent {
		return
	}
	r.Lock()
	defer r.Unlock()
	defer func() {
		if err := recover(); err != nil {
			r.err = fmt.Errorf("creative content decode: %v", err)
		}
	}()
	// The catalogue exceeds the generic 1024-element Slice limit.
	reader := protocol.NewReader(bytes.NewBuffer(payload), 0, true)
	var groups, items uint32
	reader.Varuint32(&groups)
	if groups > 4096 {
		r.err = fmt.Errorf("creative group count %d exceeds fixture limit", groups)
		return
	}
	for i := uint32(0); i < groups; i++ {
		var group protocol.CreativeGroup
		group.Marshal(reader)
	}
	reader.Varuint32(&items)
	if items > 16384 {
		r.err = fmt.Errorf("creative item count %d exceeds fixture limit", items)
		return
	}
	r.items = make([]protocol.CreativeItem, items)
	for i := range r.items {
		r.items[i].Marshal(reader)
	}
}

func (r *creativeRegistry) item(c *minecraft.Conn, name string) (protocol.CreativeItem, error) {
	var runtime int32
	found := false
	for _, entry := range c.GameData().Items {
		if entry.Name == name {
			runtime = int32(entry.RuntimeID)
			found = true
			break
		}
	}
	r.Lock()
	defer r.Unlock()
	if r.err != nil {
		return protocol.CreativeItem{}, r.err
	}
	if found {
		for _, entry := range r.items {
			if entry.Item.NetworkID == runtime {
				return entry, nil
			}
		}
	}
	return protocol.CreativeItem{}, fmt.Errorf("missing creative entry %s", name)
}

func checkCreativeInventory(c *minecraft.Conn, registry *creativeRegistry, creative bool) error {
	wantMode := int32(0)
	if creative {
		wantMode = 1
	}
	if c.GameData().PlayerGameMode != wantMode {
		return fmt.Errorf("creative test mode %d, expected %d", c.GameData().PlayerGameMode, wantMode)
	}
	entry, err := registry.item(c, "minecraft:diamond_block")
	if err != nil {
		return err
	}
	diamondRuntime, diamond := entry.Item.NetworkID, entry.CreativeItemNetworkID
	write := func(pk packet.Packet) error {
		if err := c.WritePacket(pk); err != nil {
			return err
		}
		return c.Flush()
	}
	var snapshot []protocol.ItemInstance
	filled := 0
	for n, test := range []struct {
		id                  uint32
		crafts, count, slot byte
		allowed             bool
	}{
		{diamond, 0, 64, 0, true}, {diamond, 1, 64, 1, true}, {diamond, 255, 64, 2, true},
		{diamond, 255, 255, 3, false}, {diamond, 1, 1, 1, false}, {diamond, 1, 64, 255, false},
		{0, 1, 64, 3, false}, {^uint32(0), 1, 64, 3, false},
	} {
		requestID := -int32(n + 1)
		take := &protocol.TakeStackRequestAction{}
		take.Count = test.count
		take.Source = protocol.StackRequestSlotInfo{
			Container: protocol.FullContainerName{ContainerID: protocol.ContainerCreatedOutput},
			Slot:      50, StackNetworkID: requestID,
		}
		take.Destination = protocol.StackRequestSlotInfo{
			Container: protocol.FullContainerName{ContainerID: protocol.ContainerCombinedHotBarAndInventory},
			Slot:      test.slot,
		}
		actions := []protocol.StackRequestAction{
			&protocol.CraftCreativeStackRequestAction{CreativeItemNetworkID: test.id, NumberOfCrafts: test.crafts}, take,
		}
		allowed := creative && test.allowed
		if creative && !allowed {
			// A rejected creative action must not leave a preceding inventory move applied.
			actions = append([]protocol.StackRequestAction{&protocol.SwapStackRequestAction{
				Source:      protocol.StackRequestSlotInfo{Container: take.Destination.Container, Slot: 0, StackNetworkID: snapshot[0].StackNetworkID},
				Destination: protocol.StackRequestSlotInfo{Container: take.Destination.Container, Slot: 8},
			}}, actions...)
		}
		if err := write(&packet.ItemStackRequest{Requests: []protocol.ItemStackRequest{{RequestID: requestID, Actions: actions}}}); err != nil {
			return err
		}
		c.SetReadDeadline(time.Now().Add(6 * time.Second))
		responded := false
		for j := 0; j < 4096 && !responded; j++ {
			pk, err := c.ReadPacket()
			if err != nil {
				return err
			}
			if response, ok := pk.(*packet.ItemStackResponse); ok {
				for _, entry := range response.Responses {
					if entry.RequestID == requestID {
						if (entry.Status == protocol.ItemStackResponseStatusOK) != allowed {
							return fmt.Errorf("creative=%t id=%d crafts=%d: status %d, allowed=%t", creative, test.id, test.crafts, entry.Status, allowed)
						}
						responded = true
					}
				}
			}
		}
		if !responded {
			return fmt.Errorf("missing creative inventory response")
		}
		if allowed {
			filled++
		}
		// Read the authoritative inventory after rejected requests as well as successful ones.
		if err := write(&packet.BlockPickRequest{Position: protocol.BlockPos{2, -60, 2}, HotBarSlot: 9}); err != nil {
			return err
		}
		hotbar, synced := false, false
		for j := 0; j < 4096 && !synced; j++ {
			pk, err := c.ReadPacket()
			if err != nil {
				return err
			}
			switch v := pk.(type) {
			case *packet.PlayerHotBar:
				hotbar = v.WindowID == 0 && v.SelectHotBarSlot
			case *packet.InventoryContent:
				if hotbar && v.WindowID == 0 && len(v.Content) == 36 {
					for slot, item := range v.Content {
						if slot < filled {
							if item.Stack.NetworkID != diamondRuntime || item.Stack.Count != 64 {
								return fmt.Errorf("creative=%t crafts=%d: slot %d count=%d item=%d", creative, test.crafts, slot, item.Stack.Count, item.Stack.NetworkID)
							}
						} else if item.Stack.Count != 0 {
							return fmt.Errorf("rejected creative request changed slot %d", slot)
						}
					}
					snapshot, synced = v.Content, true
				}
			}
		}
		if !synced {
			return fmt.Errorf("missing creative inventory correction")
		}
		fmt.Printf("CREATIVE mode=%d id=%d crafts=%d allowed=%t stacks=%d\n", wantMode, test.id, test.crafts, allowed, filled)
	}
	fmt.Println("PASS: creative mode permission, catalogue IDs and single-stack counts; rejected request keeps inventory")
	return nil
}
