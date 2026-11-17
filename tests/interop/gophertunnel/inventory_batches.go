package main

import (
	"fmt"

	"github.com/sandertv/gophertunnel/minecraft"
	"github.com/sandertv/gophertunnel/minecraft/protocol"
)

// Exercise real native wire batches before the ordinary mixed-edition fixture.
// The inventory is empty on entry and on return; no default gameplay is added.
func (s *nativeInventoryState) batches(conn *minecraft.Conn) (int32, error) {
	id := int32(0)
	request := func(actions ...protocol.StackRequestAction) protocol.ItemStackRequest {
		id--
		return protocol.ItemStackRequest{RequestID: id, Actions: actions, FilterCause: -1}
	}
	seed := request(
		&protocol.CraftCreativeStackRequestAction{CreativeItemNetworkID: 3, NumberOfCrafts: 1},
		nativeInventoryMove(nativeInventoryReference(protocol.ContainerCreatedOutput, 50, -1), nativeInventoryReference(protocol.ContainerInventory, 1, 0), 36, false),
		&protocol.DestroyStackRequestAction{Source: nativeInventoryReference(protocol.ContainerCreatedOutput, 50, -1), Count: 28},
	)
	if err := s.exchange(conn, seed, true); err != nil {
		return id, err
	}
	for _, size := range []int{2, 36, 64, 65} {
		before := *s
		actions := make([]protocol.StackRequestAction, 0, size)
		for offset := 0; offset < size; offset++ {
			left, right := s.main[1].netID, s.main[2].netID
			if offset%2 == 1 {
				left, right = right, left
			}
			actions = append(actions, &protocol.SwapStackRequestAction{
				Source:      nativeInventoryReference(protocol.ContainerInventory, 1, left),
				Destination: nativeInventoryReference(protocol.ContainerInventory, 2, right),
			})
		}
		if err := s.exchange(conn, request(actions...), size <= 64); err != nil {
			return id, err
		}
		if s.main != before.main || s.cursor != before.cursor || s.output != before.output {
			return id, fmt.Errorf("batch %d changed counts or identities", size)
		}
	}
	actions := make([]protocol.StackRequestAction, 0, 36)
	source := s.reference(protocol.ContainerInventory, 1)
	actions = append(actions, nativeInventoryMove(source, s.reference(protocol.ContainerCursor, 0), 1, false))
	for slot := byte(0); slot < 36; slot++ {
		if slot != 1 {
			actions = append(actions, nativeInventoryMove(source, s.reference(protocol.ContainerInventory, slot), 1, false))
		}
	}
	split := request(actions...)
	if err := s.exchange(conn, split, true); err != nil {
		return id, err
	}
	if s.main[1].count != 0 || s.cursor.count != 1 {
		return id, fmt.Errorf("full inventory split source/cursor")
	}
	for slot, stack := range s.main {
		if slot != 1 && (stack.count != 1 || stack.item != 2) {
			return id, fmt.Errorf("split slot %d: %+v", slot, stack)
		}
	}
	before := *s
	if err := s.exchange(conn, split, true); err != nil {
		return id, err
	}
	if s.main != before.main || s.cursor != before.cursor || s.output != before.output {
		return id, fmt.Errorf("batch replay changed current owner state")
	}

	actions = nil
	for slot := byte(0); slot < 36; slot++ {
		if slot != 1 {
			actions = append(actions, nativeInventoryMove(s.reference(protocol.ContainerInventory, slot), s.reference(protocol.ContainerCursor, 0), 1, true))
		}
	}
	cursorID := s.cursor.netID
	if err := s.exchange(conn, request(actions...), true); err != nil {
		return id, err
	}
	if s.cursor.count != 36 || s.cursor.netID != cursorID {
		return id, fmt.Errorf("full inventory merge lost cursor identity/count")
	}
	for _, stack := range s.main {
		if stack.count != 0 {
			return id, fmt.Errorf("full inventory merge left items")
		}
	}

	// A valid partial move followed by an invalid action must not commit IDs or counts.
	if err := s.exchange(conn, request(
		nativeInventoryMove(s.reference(protocol.ContainerCursor, 0), s.reference(protocol.ContainerInventory, 3), 1, true),
		&protocol.DestroyStackRequestAction{Source: nativeInventoryReference(99, 0, 0), Count: 1},
	), false); err != nil {
		return id, err
	}
	if err := s.exchange(conn, request(&protocol.DestroyStackRequestAction{Source: s.reference(protocol.ContainerCursor, 0), Count: 36}), true); err != nil {
		return id, err
	}
	fmt.Println("PASS: native positive-ID batches 2/36/64, 65-action rejection, full inventory split/merge, rollback and replay")
	return id, nil
}
