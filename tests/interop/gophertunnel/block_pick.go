package main

import (
	"bufio"
	"fmt"
	"github.com/sandertv/gophertunnel/minecraft"
	"github.com/sandertv/gophertunnel/minecraft/protocol"
	"github.com/sandertv/gophertunnel/minecraft/protocol/packet"
	"os"
)

func blockPick(conn *minecraft.Conn) error {
	s := nativeInventoryState{}
	pick := func(position protocol.BlockPos, hint byte, selected byte, item int32, count uint16, accepted bool) error {
		before := s
		if err := conn.WritePacket(&packet.BlockPickRequest{Position: position, HotBarSlot: hint}); err != nil {
			return err
		}
		if err := conn.Flush(); err != nil {
			return err
		}
		gui, main, held := false, false, false
		for i := 0; i < 256; i++ {
			pk, err := conn.ReadPacket()
			if err != nil {
				return err
			}
			if err := s.observe(conn, pk); err != nil {
				return err
			}
			switch p := pk.(type) {
			case *packet.GUIDataPickItem:
				if !accepted || p.HotBarSlot != int32(selected) || p.ItemEffects != "" {
					return fmt.Errorf("invalid pick UI hint %+v", p)
				}
				names := map[int32]string{1: "tile.stone.stone.name", 2: "tile.grass.name", 3: "tile.dirt.default.name", 7: "tile.bedrock.name"}
				if p.ItemName != names[item] {
					return fmt.Errorf("invalid pick item translation %s", p.ItemName)
				}
				gui = true
			case *packet.InventoryContent:
				main = main || p.WindowID == 0
			case *packet.MobEquipment:
				held = held || p.EntityRuntimeID == conn.GameData().EntityRuntimeID
			case *packet.PlayerHotBar:
				if p.SelectedHotBarSlot != uint32(selected) || p.WindowID != 0 || !p.SelectHotBarSlot || !main || !held || gui != accepted {
					return fmt.Errorf("invalid pick selection order %+v", p)
				}
				if s.selected != selected {
					return fmt.Errorf("equipment selection mismatch")
				}
				if err := s.expect(selected, item, count); err != nil {
					return err
				}
				if s.held != s.main[selected] {
					return fmt.Errorf("pick equipment differs from inventory")
				}
				if !accepted && before.loaded && (s.main != before.main || s.selected != before.selected || s.cursor != before.cursor || s.output != before.output) {
					return fmt.Errorf("rejected pick changed inventory")
				}
				ids := map[int32]bool{}
				for _, stack := range s.main {
					if stack.netID != 0 && ids[stack.netID] {
						return fmt.Errorf("duplicate pick stack ID")
					}
					if stack.netID != 0 {
						ids[stack.netID] = true
					}
				}
				return nil
			}
		}
		return fmt.Errorf("pick response exceeded packet budget")
	}
	if err := pick(protocol.BlockPos{1, 4, 1}, 0, 0, 0, 0, false); err != nil {
		return err
	}
	if err := pick(protocol.BlockPos{1, 3, 1}, 9, 0, 2, 1, true); err != nil {
		return err
	}
	initial := s.main[0]
	if err := pick(protocol.BlockPos{1, 3, 1}, 0, 0, 2, 1, true); err != nil {
		return err
	}
	if s.main[0] != initial {
		return fmt.Errorf("duplicate pick minted another stack")
	}
	if err := pick(protocol.BlockPos{63, 3, 63}, 0, 0, 2, 1, false); err != nil {
		return err
	}
	req := protocol.ItemStackRequest{RequestID: -1, FilterCause: -1, Actions: []protocol.StackRequestAction{
		&protocol.CraftCreativeStackRequestAction{CreativeItemNetworkID: 2, NumberOfCrafts: 1},
		nativeInventoryMove(nativeInventoryReference(protocol.ContainerCreatedOutput, 50, -1), s.reference(protocol.ContainerInventory, 4), 7, false),
		&protocol.DestroyStackRequestAction{Source: nativeInventoryReference(protocol.ContainerCreatedOutput, 50, -1), Count: 57},
	}}
	if err := s.exchange(conn, req, true); err != nil {
		return err
	}
	fmt.Println("BLOCK_PICK_READY")
	if _, err := bufio.NewReader(os.Stdin).ReadString('\n'); err != nil {
		return err
	}
	if err := pick(protocol.BlockPos{1, 2, 1}, 0, 4, 3, 7, true); err != nil {
		return err
	}
	if err := pick(protocol.BlockPos{2, 3, 1}, 0, 0, 2, 1, true); err != nil {
		return err
	}
	req = protocol.ItemStackRequest{RequestID: -2, FilterCause: -1, Actions: []protocol.StackRequestAction{nativeInventoryMove(s.reference(protocol.ContainerInventory, 4), s.reference(protocol.ContainerInventory, 19), 7, false)}}
	if err := s.exchange(conn, req, true); err != nil {
		return err
	}
	if err := pick(protocol.BlockPos{1, 2, 1}, 0, 0, 3, 7, true); err != nil {
		return err
	}
	if err := s.expect(19, 2, 1); err != nil {
		return err
	}
	if err := pick(protocol.BlockPos{2, 3, 1}, 9, 0, 2, 1, true); err != nil {
		return err
	}
	if err := s.expect(19, 3, 7); err != nil {
		return err
	}
	if err := pick(protocol.BlockPos{2, 3, 1}, 127, 0, 2, 1, false); err != nil {
		return err
	}
	fmt.Println("PASS: native block pick creation, reuse, selected slot, main-inventory swaps, canonical IDs and rejected hints")
	return nil
}
