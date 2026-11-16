package main

import (
	"fmt"
	"github.com/sandertv/gophertunnel/minecraft"
	"github.com/sandertv/gophertunnel/minecraft/protocol"
	"github.com/sandertv/gophertunnel/minecraft/protocol/packet"
	"time"
)

func sharedEquipment(conn *minecraft.Conn) error {
	if err := conn.SetReadDeadline(time.Now().Add(30 * time.Second)); err != nil {
		return err
	}
	own := conn.GameData().EntityRuntimeID
	ready, javaDirt, nativeGrass, late, javaEmpty := false, false, false, false, false
	for {
		pk, err := conn.ReadPacket()
		if err != nil {
			return err
		}
		switch p := pk.(type) {
		case *packet.AddPlayer:
			if p.HeldItem.Stack.NetworkID != 1 || p.HeldItem.Stack.Count != 64 {
				return fmt.Errorf("initial peer did not carry authoritative Stone: %+v", p.HeldItem)
			}
			if p.EntityRuntimeID == 1 && !ready {
				ready = true
				fmt.Println("EQUIPMENT_READY")
			} else if p.EntityRuntimeID == 3 && javaDirt && nativeGrass && !late {
				late = true
				fmt.Println("EQUIPMENT_LATE_JOINED")
			} else {
				return fmt.Errorf("unexpected equipment peer spawn")
			}
		case *packet.MobEquipment:
			if p.EntityRuntimeID == 1 {
				if !ready || p.WindowID != 0 || p.InventorySlot != p.HotBarSlot {
					return fmt.Errorf("invalid Java peer equipment")
				}
				if p.HotBarSlot == 1 && !javaDirt && p.NewItem.Stack.NetworkID == 3 && p.NewItem.Stack.Count == 64 {
					javaDirt = true
					fmt.Println("EQUIPMENT_JAVA_DIRT")
					// A valid but false item/count claim must never be broadcast.
					err = conn.WritePacket(&packet.MobEquipment{EntityRuntimeID: own, NewItem: protocol.ItemInstance{Stack: protocol.ItemStack{ItemType: protocol.ItemType{NetworkID: 1}, Count: 1}}, InventorySlot: 2, HotBarSlot: 2})
					if err != nil {
						return err
					}
					if err = conn.Flush(); err != nil {
						return err
					}
				} else if p.HotBarSlot == 8 && late && !javaEmpty && p.NewItem.Stack.NetworkID == 0 && p.NewItem.Stack.Count == 0 {
					javaEmpty = true
					fmt.Println("EQUIPMENT_JAVA_EMPTY")
					if err = conn.WritePacket(&packet.PlayerHotBar{SelectedHotBarSlot: 8, SelectHotBarSlot: true}); err != nil {
						return err
					}
					if err = conn.Flush(); err != nil {
						return err
					}
				} else {
					return fmt.Errorf("unexpected Java equipment: %+v", p)
				}
			} else if p.EntityRuntimeID == own && javaDirt {
				if p.WindowID != 0 || p.InventorySlot != p.HotBarSlot {
					return fmt.Errorf("invalid native equipment correction")
				}
				if p.HotBarSlot == 2 && !nativeGrass && p.NewItem.Stack.NetworkID == 2 && p.NewItem.Stack.Count == 64 {
					nativeGrass = true
					fmt.Println("EQUIPMENT_NATIVE_GRASS")
				} else if p.HotBarSlot == 8 && javaEmpty && p.NewItem.Stack.NetworkID == 0 && p.NewItem.Stack.Count == 0 {
					fmt.Println("EQUIPMENT_NATIVE_EMPTY")
					fmt.Println("PASS: independent Bedrock initial held item, Java/native selection, forged-item correction, late-join and empty-hand equipment")
					return nil
				} else {
					return fmt.Errorf("claimed item escaped authoritative correction: %+v", p)
				}
			}
		}
	}
}
