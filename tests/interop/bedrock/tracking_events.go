package main

import (
	"fmt"
	"time"

	"github.com/sandertv/gophertunnel/minecraft"
	"github.com/sandertv/gophertunnel/minecraft/protocol/packet"
)

func checkTrackingEvents(c *minecraft.Conn, motion bool) error {
	const barrier = int64(0x121314151617181)
	const trigger = int64(0x212223242526272)
	removed := false
	var near int64
	c.SetReadDeadline(time.Now().Add(6 * time.Second))
	for n := 0; n < 4096; n++ {
		pk, err := c.ReadPacket()
		if err != nil {
			return fmt.Errorf("tracker callback failed to finish: %w", err)
		}
		switch v := pk.(type) {
		case *packet.RemoveActor:
			if motion && (near == 0 || v.EntityUniqueID != near) {
				continue
			}
			removed = true
			if err := c.WritePacket(&packet.NetworkStackLatency{Timestamp: barrier}); err != nil {
				return err
			}
			if err := c.Flush(); err != nil {
				return err
			}
		case *packet.AddActor:
			if (v.EntityType == "minecraft:pig" || v.EntityType == "pig") && v.Position.X() < 6 {
				if !motion {
					return fmt.Errorf("cancelled mob spawn reached the client")
				}
				near = v.EntityUniqueID
				if err := c.WritePacket(&packet.NetworkStackLatency{Timestamp: trigger}); err != nil {
					return err
				}
				if err := c.Flush(); err != nil {
					return err
				}
			}
		case *packet.SetActorMotion:
			if motion && v.EntityRuntimeID == uint64(near) {
				return fmt.Errorf("cancelled mob motion reached the client")
			}
		case *packet.NetworkStackLatency:
			if removed && v.Timestamp == barrier && !v.NeedsResponse {
				fmt.Println("PASS: outgoing tracking callback removed the mob; packet cancelled; transport still responds")
				return nil
			}
		}
	}
	return fmt.Errorf("tracker callback exceeded packet budget")
}
