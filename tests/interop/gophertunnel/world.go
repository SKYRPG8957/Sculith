package main

import (
	"fmt"
	"github.com/df-mc/dragonfly/server/world"
	"github.com/sandertv/gophertunnel/minecraft"
	"github.com/sandertv/gophertunnel/minecraft/protocol"
	"github.com/sandertv/gophertunnel/minecraft/protocol/packet"
	"time"
)

// A fresh native peer observes the same owner commits as the Java controller.
func sharedPluginWorld(conn *minecraft.Conn, registry world.BlockRegistry) error {
	if err := connectionControl(conn); err != nil {
		return err
	}
	if err := conn.SetReadDeadline(time.Now().Add(20 * time.Second)); err != nil {
		return err
	}
	fmt.Println("WORLD_NATIVE_READY")
	stages := []map[protocol.BlockPos]string{
		{{1, 4, 2}: "minecraft:stone", {17, 4, 2}: "minecraft:dirt"},
		{{1, 4, 2}: "minecraft:air", {17, 4, 2}: "minecraft:air"},
		{{2, 4, 2}: "minecraft:stone"},
	}
	labels := []string{"WORLD_NATIVE_BUILT", "WORLD_NATIVE_ERASED", "WORLD_NATIVE_PLAYER_COMMIT"}
	stage := 0
	for count := 0; count < 256; count++ {
		pk, err := conn.ReadPacket()
		if err != nil {
			return err
		}
		switch p := pk.(type) {
		case *packet.Text:
			return fmt.Errorf("private world reply leaked to native player: %s", p.Message)
		case *packet.UpdateBlock:
			expected, exists := stages[stage][p.Position]
			rid, known := registry.HashToRuntimeID(p.NewBlockRuntimeID)
			var name string
			if known {
				value, exists := registry.BlockByRuntimeID(rid)
				if exists {
					name, _ = value.EncodeBlock()
				} else {
					known = false
				}
			}
			if !exists || !known || name != expected {
				return fmt.Errorf("unexpected world delta at stage%d: %+v name=%s", stage, p, name)
			}
			delete(stages[stage], p.Position)
			if len(stages[stage]) == 0 {
				fmt.Println(labels[stage])
				stage++
				if stage == len(stages) {
					fmt.Println("PASS: native peer received the same five committed plugin/player block changes")
					return nil
				}
			}
		}
	}
	return fmt.Errorf("world delta packet budget exceeded")
}
