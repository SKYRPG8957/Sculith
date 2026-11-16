package main

import (
	"fmt"
	"time"

	"github.com/google/uuid"
	"github.com/sandertv/gophertunnel/minecraft"
	"github.com/sandertv/gophertunnel/minecraft/protocol"
	"github.com/sandertv/gophertunnel/minecraft/protocol/packet"
)

func pluginInventory(conn *minecraft.Conn) error {
	s := new(nativeInventoryState)
	if err := s.exchange(conn, fixtureInventoryRequest(), true); err != nil {
		return err
	}
	if err := s.exchange(conn, protocol.ItemStackRequest{RequestID: -2, FilterCause: -1,
		Actions: []protocol.StackRequestAction{&protocol.CraftReservedStackRequestAction{ReservedID: "sculith:unsupported", NumberOfCrafts: 1}}}, false); err != nil {
		return err
	}
	destroy := new(protocol.DestroyStackRequestAction)
	destroy.Source, destroy.Count = s.reference(protocol.ContainerInventory, 0), 64
	if err := s.exchange(conn, protocol.ItemStackRequest{RequestID: -3, FilterCause: -1, Actions: []protocol.StackRequestAction{destroy}}, true); err != nil {
		return err
	}
	command := func(line string) error {
		if err := conn.WritePacket(&packet.CommandRequest{CommandLine: line, CommandOrigin: protocol.CommandOrigin{Origin: protocol.CommandOriginPlayer, UUID: uuid.New()}}); err != nil {
			return err
		}
		return conn.Flush()
	}
	if err := command("/sculith_test_give"); err != nil {
		return err
	}
	if err := conn.SetReadDeadline(time.Now().Add(12 * time.Second)); err != nil {
		return err
	}
	granted, javaGranted, querySent := false, false, false
	contentsAfterGrant := 0
	for index := 0; index < 1024; index++ {
		pk, err := conn.ReadPacket()
		if err != nil {
			return err
		}
		if err = s.observe(conn, pk); err != nil {
			return err
		}
		switch p := pk.(type) {
		case *packet.InventoryContent:
			if granted && p.WindowID == protocol.WindowIDInventory {
				contentsAfterGrant++
				return fmt.Errorf("foreign inventory push reached native player (%d)", contentsAfterGrant)
			}
		case *packet.Text:
			if p.TextType != packet.TextTypeSystem {
				continue
			}
			switch p.Message {
			case "Added 5 Dirt.":
				if granted {
					return fmt.Errorf("Java private grant reply leaked to native player")
				}
				if err = s.expect(0, 3, 5); err != nil {
					return err
				}
				if s.held != s.main[0] || s.cursor.count != 0 || s.output.count != 0 {
					return fmt.Errorf("native grant state mismatch")
				}
				granted = true
				fmt.Println("PLUGIN_INVENTORY_NATIVE_GRANTED")
			case "Inventory: 3 stacks, 133 items.":
				if !granted || !javaGranted || !querySent {
					return fmt.Errorf("inventory response arrived before its request")
				}
				fmt.Println("PASS: native plugin grant/query, counted own/Java hands and private inventory responses")
				return nil
			default:
				return fmt.Errorf("unexpected native system text: %s", p.Message)
			}
		case *packet.MobEquipment:
			if p.EntityRuntimeID == 1 && p.HotBarSlot == 0 && p.NewItem.Stack.NetworkID == 3 && p.NewItem.Stack.Count == 5 {
				javaGranted = true
				fmt.Println("PLUGIN_INVENTORY_JAVA_GRANTED")
				if !querySent {
					if err = command("/sculith_test_inventory"); err != nil {
						return err
					}
					querySent = true
				}
			}
		}
	}
	return fmt.Errorf("plugin inventory packet budget exceeded")
}
