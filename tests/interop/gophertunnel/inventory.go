package main

import (
	"fmt"
	"time"

	"github.com/sandertv/gophertunnel/minecraft"
	"github.com/sandertv/gophertunnel/minecraft/protocol"
	"github.com/sandertv/gophertunnel/minecraft/protocol/packet"
)

// This probe uses the independent pinned native codec and real sockets. It does
// not verify the official GDK client's inventory rendering or input behavior.
type nativeInventoryStack struct {
	item  int32
	count uint16
	netID int32
}

type nativeInventoryState struct {
	selected     byte
	main         [36]nativeInventoryStack
	cursor       nativeInventoryStack
	output       nativeInventoryStack
	held         nativeInventoryStack
	loaded       bool
	sawJavaGrass bool
	sawCatalogue bool
}

type nativeInventorySlot struct {
	container byte
	slot      byte
}

func nativeStack(item protocol.ItemInstance) (nativeInventoryStack, error) {
	s := nativeInventoryStack{item.Stack.NetworkID, item.Stack.Count, item.StackNetworkID}
	if s.item == 0 {
		if s.count != 0 || s.netID != 0 {
			return s, fmt.Errorf("invalid empty inventory stack: %+v", s)
		}
		return s, nil
	}
	if (s.item != 1 && s.item != 3 && s.item != 2 && s.item != 7) || s.count < 1 || s.count > 64 || s.netID <= 0 {
		return s, fmt.Errorf("invalid authoritative inventory stack: %+v", s)
	}
	return s, nil
}

func (s *nativeInventoryState) reference(container, slot byte) protocol.StackRequestSlotInfo {
	var item nativeInventoryStack
	switch container {
	case protocol.ContainerInventory, protocol.ContainerHotBar, protocol.ContainerCombinedHotBarAndInventory:
		item = s.main[slot]
	case protocol.ContainerCursor:
		item = s.cursor
	case protocol.ContainerCreatedOutput:
		item = s.output
	}
	return nativeInventoryReference(container, slot, item.netID)
}

func nativeInventoryReference(container, slot byte, id int32) protocol.StackRequestSlotInfo {
	return protocol.StackRequestSlotInfo{
		Container: protocol.FullContainerName{ContainerID: container},
		Slot:      slot, StackNetworkID: id,
	}
}

func nativeInventoryMove(source, destination protocol.StackRequestSlotInfo, count byte, place bool) protocol.StackRequestAction {
	if place {
		a := new(protocol.PlaceStackRequestAction)
		a.Source, a.Destination, a.Count = source, destination, count
		return a
	}
	a := new(protocol.TakeStackRequestAction)
	a.Source, a.Destination, a.Count = source, destination, count
	return a
}

func (s *nativeInventoryState) observe(conn *minecraft.Conn, pk packet.Packet) error {
	switch p := pk.(type) {
	case *packet.InventoryContent:
		if p.WindowID != protocol.WindowIDInventory {
			return nil
		}
		_, dynamic := p.Container.DynamicContainerID.Value()
		if len(p.Content) != len(s.main) || p.Container.ContainerID != protocol.ContainerInventory || dynamic {
			return fmt.Errorf("invalid main inventory resync")
		}
		for index, item := range p.Content {
			stack, err := nativeStack(item)
			if err != nil {
				return err
			}
			s.main[index] = stack
		}
		s.loaded = true
	case *packet.InventorySlot:
		if p.WindowID != protocol.WindowIDUI || (p.Slot != 0 && p.Slot != 50) {
			return nil
		}
		name, present := p.Container.Value()
		_, dynamic := name.DynamicContainerID.Value()
		expected := byte(protocol.ContainerCursor)
		if p.Slot == 50 {
			expected = protocol.ContainerCreatedOutput
		}
		if !present || dynamic || name.ContainerID != expected {
			return fmt.Errorf("invalid UI inventory container at slot %d", p.Slot)
		}
		stack, err := nativeStack(p.NewItem)
		if err != nil {
			return err
		}
		if p.Slot == 0 {
			s.cursor = stack
		} else {
			s.output = stack
		}
	case *packet.MobEquipment:
		if p.EntityRuntimeID == conn.GameData().EntityRuntimeID {
			if p.WindowID != 0 || p.InventorySlot != p.HotBarSlot || p.HotBarSlot > 8 {
				return fmt.Errorf("native selected slot is invalid")
			}
			stack, err := nativeStack(p.NewItem)
			if err != nil {
				return err
			}
			s.held = stack
			s.selected = p.HotBarSlot
		} else if p.EntityRuntimeID == 1 && p.HotBarSlot == 0 && p.WindowID == 0 && p.NewItem.Stack.NetworkID == 2 && p.NewItem.Stack.Count == 2 {
			if p.InventorySlot != p.HotBarSlot {
				return fmt.Errorf("Java peer equipment slot mismatch")
			}
			s.sawJavaGrass = true
		}
	case *packet.CreativeContent:
		expected := map[uint32]int32{1: 1, 2: 3, 3: 2, 4: 7}
		if len(p.Items) != 4 || len(p.Groups) != 1 {
			return fmt.Errorf("invalid native creative catalogue")
		}
		for _, item := range p.Items {
			want, ok := expected[item.CreativeItemNetworkID]
			if !ok || item.Item.NetworkID != want || item.Item.Count != 1 || item.GroupIndex != 0 {
				return fmt.Errorf("creative catalogue ID mismatch")
			}
			delete(expected, item.CreativeItemNetworkID)
		}
		s.sawCatalogue = true
	}
	return nil
}

func (s *nativeInventoryState) exchange(conn *minecraft.Conn, req protocol.ItemStackRequest, success bool) error {
	before := *s
	if err := conn.SetReadDeadline(time.Now().Add(8 * time.Second)); err != nil {
		return err
	}
	if err := conn.WritePacket(&packet.ItemStackRequest{Requests: []protocol.ItemStackRequest{req}}); err != nil {
		return err
	}
	if err := conn.Flush(); err != nil {
		return err
	}
	responseSeen, mainSeen, heldSeen, cursorSeen, outputSeen := false, false, false, false, false
	changes := map[nativeInventorySlot]protocol.StackResponseSlotInfo{}
	for packets := 0; packets < 512; packets++ {
		pk, err := conn.ReadPacket()
		if err != nil {
			return fmt.Errorf("inventory request %d: %w", req.RequestID, err)
		}
		if err := s.observe(conn, pk); err != nil {
			return err
		}
		switch p := pk.(type) {
		case *packet.ItemStackResponse:
			if responseSeen || len(p.Responses) != 1 {
				return fmt.Errorf("duplicate or batched unexpected inventory response")
			}
			r := p.Responses[0]
			if r.RequestID != req.RequestID || (r.Status == protocol.ItemStackResponseStatusOK) != success || (!success && r.Status != protocol.ItemStackResponseStatusError) {
				return fmt.Errorf("request %d status=%d expected success=%v response ID=%d", req.RequestID, r.Status, success, r.RequestID)
			}
			if !success && len(r.ContainerInfo) != 0 {
				return fmt.Errorf("rejection contained success deltas")
			}
			for _, container := range r.ContainerInfo {
				_, dynamic := container.Container.DynamicContainerID.Value()
				if dynamic {
					return fmt.Errorf("dynamic response container")
				}
				for _, slot := range container.SlotInfo {
					key := nativeInventorySlot{container.Container.ContainerID, slot.Slot}
					if _, duplicate := changes[key]; duplicate {
						return fmt.Errorf("duplicate inventory response slot")
					}
					// Pinned Dragonfly uses the same absolute index for both
					// fields, including non-hotbar slots 9..35 (no modulo nine).
					if slot.HotbarSlot != slot.Slot || slot.Count > 64 || (slot.Count == 0 && slot.StackNetworkID != 0) || (slot.Count != 0 && slot.StackNetworkID <= 0) {
						return fmt.Errorf("invalid response slot/hotbar/count/ID: %+v", slot)
					}
					changes[key] = slot
				}
			}
			responseSeen = true
			mainSeen, heldSeen, cursorSeen, outputSeen = false, false, false, false
		case *packet.InventoryContent:
			mainSeen = mainSeen || (responseSeen && p.WindowID == protocol.WindowIDInventory)
		case *packet.MobEquipment:
			heldSeen = heldSeen || (responseSeen && p.EntityRuntimeID == conn.GameData().EntityRuntimeID)
		case *packet.InventorySlot:
			cursorSeen = cursorSeen || (responseSeen && p.WindowID == protocol.WindowIDUI && p.Slot == 0)
			outputSeen = outputSeen || (responseSeen && p.WindowID == protocol.WindowIDUI && p.Slot == 50)
		}
		if responseSeen && mainSeen && heldSeen && cursorSeen && outputSeen {
			if s.selected != before.selected || s.held != s.main[s.selected] {
				return fmt.Errorf("held equipment differs from selected owner slot")
			}
			ids := map[int32]bool{}
			for _, stack := range append(s.main[:], s.cursor, s.output) {
				if stack.netID != 0 && ids[stack.netID] {
					return fmt.Errorf("duplicate authoritative stack identity")
				}
				ids[stack.netID] = true
			}
			if before.loaded && !success && (s.main != before.main || s.cursor != before.cursor || s.output != before.output) {
				return fmt.Errorf("rejected request %d changed authoritative state or IDs", req.RequestID)
			}
			for key, expected := range changes {
				var actual nativeInventoryStack
				switch key.container {
				case protocol.ContainerInventory:
					if key.slot >= 36 {
						return fmt.Errorf("response inventory slot out of bounds")
					}
					actual = s.main[key.slot]
				case protocol.ContainerCursor:
					if key.slot != 0 {
						return fmt.Errorf("response cursor slot out of bounds")
					}
					actual = s.cursor
				case protocol.ContainerCreatedOutput:
					if key.slot != 50 {
						return fmt.Errorf("response output slot out of bounds")
					}
					actual = s.output
				default:
					return fmt.Errorf("unexpected canonical response container %d", key.container)
				}
				if actual.count != uint16(expected.Count) || actual.netID != expected.StackNetworkID {
					return fmt.Errorf("response/resync mismatch at %+v", key)
				}
			}
			return nil
		}
	}
	return fmt.Errorf("inventory request exceeded packet budget")
}

func (s *nativeInventoryState) expect(slot byte, item int32, count uint16) error {
	actual := s.main[slot]
	if actual.item != item || actual.count != count {
		return fmt.Errorf("inventory slot %d: got %+v expected item%d count%d", slot, actual, item, count)
	}
	return nil
}

func fixtureInventoryRequest() protocol.ItemStackRequest {
	return fixtureInventoryRequestID(-1)
}

func fixtureInventoryRequestID(id int32) protocol.ItemStackRequest {
	request := protocol.ItemStackRequest{RequestID: id, FilterCause: -1}
	for index := 0; index < 3; index++ {
		request.Actions = append(request.Actions,
			&protocol.CraftCreativeStackRequestAction{CreativeItemNetworkID: uint32(index + 1), NumberOfCrafts: 1},
			nativeInventoryMove(nativeInventoryReference(protocol.ContainerCreatedOutput, 50, id), nativeInventoryReference(protocol.ContainerInventory, byte(index), 0), 64, false))
	}
	return request
}

func seedFixtureInventory(conn *minecraft.Conn) error {
	if err := conn.WritePacket(&packet.ItemStackRequest{Requests: []protocol.ItemStackRequest{fixtureInventoryRequest()}}); err != nil {
		return err
	}
	return conn.Flush()
}

func sharedInventory(conn *minecraft.Conn) error {
	s := new(nativeInventoryState)
	id, err := s.batches(conn)
	if err != nil {
		return err
	}
	id--
	if err := s.exchange(conn, fixtureInventoryRequestID(id), true); err != nil {
		return err
	}
	request := func(actions ...protocol.StackRequestAction) protocol.ItemStackRequest {
		id--
		return protocol.ItemStackRequest{RequestID: id, Actions: actions, FilterCause: -1}
	}
	if err := s.exchange(conn, request(&protocol.CraftReservedStackRequestAction{ReservedID: "sculith:unsupported", NumberOfCrafts: 1}), false); err != nil {
		return err
	}
	for slot, item := range []int32{1, 3, 2} {
		if err := s.expect(byte(slot), item, 64); err != nil {
			return err
		}
	}
	if s.cursor.count != 0 || s.output.count != 0 {
		return fmt.Errorf("initial UI slots were occupied")
	}
	stale := s.reference(protocol.ContainerInventory, 0)
	for index, item := range []int32{1, 3, 2, 7} {
		next := id - 1
		req := request(
			&protocol.CraftCreativeStackRequestAction{CreativeItemNetworkID: uint32(index + 1), NumberOfCrafts: 1},
			nativeInventoryMove(nativeInventoryReference(protocol.ContainerCreatedOutput, 50, next), s.reference(protocol.ContainerInventory, byte(9+index)), 64, false),
		)
		if err := s.exchange(conn, req, true); err != nil {
			return err
		}
		if err := s.expect(byte(9+index), item, 64); err != nil {
			return err
		}
		if s.output.count != 0 {
			return fmt.Errorf("creative output was not fully transferred")
		}
	}
	if err := s.exchange(conn, request(nativeInventoryMove(s.reference(protocol.ContainerInventory, 9), s.reference(protocol.ContainerCursor, 0), 17, false)), true); err != nil {
		return err
	}
	previous := id
	if s.cursor.count != 17 {
		return fmt.Errorf("split did not produce cursor17")
	}
	if err := s.expect(9, 1, 47); err != nil {
		return err
	}
	if err := s.exchange(conn, request(nativeInventoryMove(nativeInventoryReference(protocol.ContainerCursor, 0, previous), s.reference(protocol.ContainerCombinedHotBarAndInventory, 13), 7, true)), true); err != nil {
		return err
	}
	previous = id
	if err := s.exchange(conn, request(nativeInventoryMove(nativeInventoryReference(protocol.ContainerCursor, 0, previous), nativeInventoryReference(protocol.ContainerInventory, 13, previous), 10, true)), true); err != nil {
		return err
	}
	if err := s.expect(13, 1, 17); err != nil {
		return err
	}
	if s.cursor.count != 0 {
		return fmt.Errorf("merge left cursor occupied")
	}
	if err := s.exchange(conn, request(&protocol.SwapStackRequestAction{Source: s.reference(protocol.ContainerInventory, 13), Destination: s.reference(protocol.ContainerInventory, 10)}), true); err != nil {
		return err
	}
	if err := s.expect(13, 3, 64); err != nil {
		return err
	}
	if err := s.expect(10, 1, 17); err != nil {
		return err
	}
	if err := s.exchange(conn, request(&protocol.DestroyStackRequestAction{Source: s.reference(protocol.ContainerInventory, 13), Count: 59}), true); err != nil {
		return err
	}
	if err := s.expect(13, 3, 5); err != nil {
		return err
	}
	if err := s.exchange(conn, request(nativeInventoryMove(s.reference(protocol.ContainerHotBar, 0), s.reference(protocol.ContainerInventory, 14), 64, true)), true); err != nil {
		return err
	}
	finalSwap := request(&protocol.SwapStackRequestAction{Source: s.reference(protocol.ContainerInventory, 13), Destination: s.reference(protocol.ContainerHotBar, 0)})
	if err := s.exchange(conn, finalSwap, true); err != nil {
		return err
	}
	if err := s.expect(0, 3, 5); err != nil {
		return err
	}
	for _, req := range []protocol.ItemStackRequest{
		request(&protocol.DestroyStackRequestAction{Source: stale, Count: 1}),
		request(&protocol.CraftCreativeStackRequestAction{CreativeItemNetworkID: 999, NumberOfCrafts: 1}),
		// A valid first action followed by an unknown-container action must
		// roll back the temporary output as well as its allocated stack ID.
		request(&protocol.CraftCreativeStackRequestAction{CreativeItemNetworkID: 1, NumberOfCrafts: 1}, &protocol.DestroyStackRequestAction{Source: nativeInventoryReference(99, 0, s.main[0].netID), Count: 1}),
		request(&protocol.DropStackRequestAction{Source: s.reference(protocol.ContainerInventory, 0), Count: 1}),
	} {
		if err := s.exchange(conn, req, false); err != nil {
			return err
		}
	}
	before := *s
	if err := s.exchange(conn, finalSwap, true); err != nil {
		return err
	}
	if s.main != before.main || s.cursor != before.cursor || s.output != before.output {
		return fmt.Errorf("request replay duplicated effects or allocated new IDs")
	}
	if err := s.exchange(conn, request(&protocol.MineBlockStackRequestAction{HotbarSlot: 0, PredictedDurability: 0, StackNetworkID: s.main[0].netID}), true); err != nil {
		return err
	}
	if err := s.expect(0, 3, 5); err != nil {
		return err
	}
	if s.cursor.count != 0 || s.output.count != 0 {
		return fmt.Errorf("final UI inventory occupied")
	}
	totals := map[int32]uint16{}
	for _, stack := range s.main {
		totals[stack.item] += stack.count
	}
	for item, expected := range map[int32]uint16{1: 128, 3: 69, 2: 128, 7: 64} {
		if totals[item] != expected {
			return fmt.Errorf("item%d count conservation: got%d expected%d", item, totals[item], expected)
		}
	}

	// An explicit control reply after unsupported Drop proves continued usability.
	stamp := time.Now().UnixNano()
	if err := conn.WritePacket(&packet.NetworkStackLatency{Timestamp: stamp, NeedsResponse: true}); err != nil {
		return err
	}
	if err := conn.Flush(); err != nil {
		return err
	}
	latency := false
	for packets := 0; packets < 256 && !latency; packets++ {
		pk, err := conn.ReadPacket()
		if err != nil {
			return err
		}
		if err := s.observe(conn, pk); err != nil {
			return err
		}
		if p, ok := pk.(*packet.NetworkStackLatency); ok && p.Timestamp == stamp && !p.NeedsResponse {
			latency = true
		}
	}
	if !latency {
		return fmt.Errorf("missing post-rejection control reply")
	}
	fmt.Println("INVENTORY_NATIVE_READY")
	if err := conn.SetReadDeadline(time.Now().Add(35 * time.Second)); err != nil {
		return err
	}
	for packets := 0; packets < 1024 && !s.sawJavaGrass; packets++ {
		pk, err := conn.ReadPacket()
		if err != nil {
			return err
		}
		if err := s.observe(conn, pk); err != nil {
			return err
		}
	}
	if !s.sawJavaGrass {
		return fmt.Errorf("missing Java peer Grass2 equipment")
	}
	fmt.Println("INVENTORY_JAVA_HELD_READY")
	// Pickup/swap, main/hotbar moves and cursor collection publish all committed
	// held changes, including the empty hand, to the other edition.
	for _, want := range []struct {
		item  int32
		count uint16
	}{{2, 1}, {3, 64}, {0, 0}, {3, 34}, {3, 64}, {3, 34}} {
		seen := false
		for packets := 0; packets < 256; packets++ {
			pk, err := conn.ReadPacket()
			if err != nil {
				return err
			}
			p, ok := pk.(*packet.MobEquipment)
			if !ok || p.EntityRuntimeID != 1 {
				continue
			}
			if p.HotBarSlot != 0 || p.InventorySlot != 0 || p.WindowID != 0 ||
				p.NewItem.Stack.NetworkID != want.item || p.NewItem.Stack.Count != want.count {
				return fmt.Errorf("Java inventory peer equipment: got %+v want item%d count%d", p, want.item, want.count)
			}
			seen = true
			break
		}
		if !seen {
			return fmt.Errorf("missing Java inventory peer equipment")
		}
	}
	fmt.Println("PASS: independent native creative creation, counts, predictions, rejections, replay, resync and Java counted inventory hands")
	return nil
}
