package main

import (
	"fmt"
	"math"
	"time"

	"github.com/sandertv/gophertunnel/minecraft/protocol"
	"github.com/sandertv/gophertunnel/minecraft/protocol/packet"
)

func checkTerrainTravel(address string) error {
	c, err := dialSingle(address, "TerrainProbe", nil)
	if err != nil {
		return err
	}
	defer c.Close()
	input := protocol.NewInputFlagsFromIDs(packet.InputFlagCount, []int32{packet.InputFlagHandledTeleport})
	var tick uint64
	for _, destination := range []protocol.ChunkPos{{32, 48}, {0, 0}} {
		fmt.Printf("TERRAIN_TELEPORT %d %d\n", destination[0], destination[1])
		c.SetReadDeadline(time.Now().Add(30 * time.Second))
		var publisher, terrain, moved bool
		for !publisher || !terrain || !moved {
			pk, err := c.ReadPacket()
			if err != nil {
				return fmt.Errorf("chunk %v publisher=%t terrain=%t moved=%t: %w", destination, publisher, terrain, moved, err)
			}
			switch p := pk.(type) {
			case *packet.MovePlayer:
				if p.EntityRuntimeID != c.GameData().EntityRuntimeID || p.Mode != packet.MoveModeTeleport {
					continue
				}
				tick++
				if err := c.WritePacket(&packet.PlayerAuthInput{Position: p.Position, Tick: tick, InputMode: packet.InputModeMouse, PlayMode: packet.PlayModeScreen, InputData: input}); err != nil {
					return err
				}
				if err := c.Flush(); err != nil {
					return err
				}
				moved = protocol.ChunkPos{int32(math.Floor(float64(p.Position[0]))) >> 4, int32(math.Floor(float64(p.Position[2]))) >> 4} == destination
			case *packet.NetworkChunkPublisherUpdate:
				publisher = protocol.ChunkPos{p.Position[0] >> 4, p.Position[2] >> 4} == destination && p.Radius >= 16*16
			case *packet.LevelChunk:
				if publisher && p.Position == destination {
					terrain = true
				}
			}
		}
		fmt.Printf("TERRAIN_RECEIVED %d %d\n", destination[0], destination[1])
	}
	return nil
}
