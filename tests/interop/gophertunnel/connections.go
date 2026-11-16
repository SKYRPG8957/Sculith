package main

import (
	"fmt"
	"time"

	"github.com/sandertv/gophertunnel/minecraft"
	"github.com/sandertv/gophertunnel/minecraft/protocol/packet"
)

// A bounded control exchange after DoSpawn proves the adapter's play phase
// without depending on another client or server gameplay commands.
func connectionControl(conn *minecraft.Conn) error {
	stamp := time.Now().UnixNano()
	if err := conn.WritePacket(&packet.NetworkStackLatency{Timestamp: stamp, NeedsResponse: true}); err != nil {
		return err
	}
	if err := conn.Flush(); err != nil {
		return err
	}
	for count := 0; count < 256; count++ {
		pk, err := conn.ReadPacket()
		if err != nil {
			return err
		}
		if reply, ok := pk.(*packet.NetworkStackLatency); ok && reply.Timestamp == stamp && !reply.NeedsResponse {
			fmt.Println("PASS: independent native post-spawn control reply")
			return nil
		}
	}
	return fmt.Errorf("missing native post-spawn control reply")
}
