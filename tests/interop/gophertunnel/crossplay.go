package main

import (
	"fmt"
	"time"

	"github.com/df-mc/dragonfly/server/world"
	"github.com/sandertv/gophertunnel/minecraft"
	"github.com/sandertv/gophertunnel/minecraft/protocol"
	"github.com/sandertv/gophertunnel/minecraft/protocol/packet"
)

// Optional manual graphical-client companion. After the normal full flat-world
// verification, break one visible block and observe Java's mouse-driven edits.
// This remains a headless Bedrock client; it is not the official Bedrock game.
func crossplay(conn *minecraft.Conn, registry world.BlockRegistry) error {
	position := protocol.BlockPos{0, 3, 2}
	if err := conn.SetReadDeadline(time.Now().Add(120 * time.Second)); err != nil {
		return err
	}
	if err := conn.WritePacket(&packet.PlayerAction{
		EntityRuntimeID: conn.GameData().EntityRuntimeID,
		ActionType:      protocol.PlayerActionPredictDestroyBlock,
		BlockPosition:   position, ResultPosition: position, BlockFace: 1,
	}); err != nil {
		return err
	}
	if err := conn.Flush(); err != nil {
		return err
	}
	for _, want := range []string{"minecraft:air", "minecraft:stone", "minecraft:air", "minecraft:stone"} {
		for {
			pk, err := conn.ReadPacket()
			if err != nil {
				return err
			}
			update, ok := pk.(*packet.UpdateBlock)
			if !ok || update.Position != position {
				continue
			}
			rid, ok := registry.HashToRuntimeID(update.NewBlockRuntimeID)
			if !ok {
				return fmt.Errorf("unrecognized committed block hash %08x", update.NewBlockRuntimeID)
			}
			name, _, _ := registry.RuntimeIDToState(rid)
			if name != want {
				return fmt.Errorf("block at %v: want %s, got %s", position, want, name)
			}
			fmt.Println("CROSSPLAY: committed", position, name)
			break
		}
	}
	fmt.Println("PASS: Bedrock break and Java place/break/place shared in one world")
	return nil
}
