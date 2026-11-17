package main

import (
	"fmt"
	"time"

	"github.com/sandertv/gophertunnel/minecraft"
	"github.com/sandertv/gophertunnel/minecraft/protocol/packet"
)

func inventoryOpen(conn *minecraft.Conn) packet.Packet {
	return &packet.Interact{ActionType: packet.InteractActionOpenInventory, TargetEntityRuntimeID: conn.GameData().EntityRuntimeID}
}

func inventoryClose(window byte) packet.Packet {
	return &packet.ContainerClose{WindowID: window, ContainerType: 255, ServerSide: false}
}

// A following ordered control reply fences the complete batch, including cases
// where duplicates and stale notifications must produce no window response.
func (s *nativeInventoryState) windowRoundTrip(conn *minecraft.Conn, outbound []packet.Packet, want string, window byte) error {
	before := *s
	stamp := time.Now().UnixNano()
	if err := conn.SetReadDeadline(time.Now().Add(8 * time.Second)); err != nil {
		return err
	}
	for _, p := range outbound {
		if err := conn.WritePacket(p); err != nil {
			return err
		}
	}
	if err := conn.WritePacket(&packet.NetworkStackLatency{Timestamp: stamp, NeedsResponse: true}); err != nil {
		return err
	}
	if err := conn.Flush(); err != nil {
		return err
	}
	replies := 0
	for frame := 0; frame < 256; frame++ {
		p, err := conn.ReadPacket()
		if err != nil {
			return err
		}
		if err = s.observe(conn, p); err != nil {
			return err
		}
		switch p := p.(type) {
		case *packet.ContainerOpen:
			replies++
			if want != "open" || p.WindowID != window || p.ContainerType != 255 || p.ContainerEntityUniqueID != -1 {
				return fmt.Errorf("unexpected inventory open %+v", p)
			}
		case *packet.ContainerClose:
			replies++
			if want != "close" || p.WindowID != window || p.ContainerType != 255 || p.ServerSide {
				return fmt.Errorf("unexpected inventory close %+v", p)
			}
		case *packet.NetworkStackLatency:
			if p.Timestamp == stamp && !p.NeedsResponse {
				expected := 0
				if want != "" {
					expected = 1
				}
				if replies != expected {
					return fmt.Errorf("window %s id=%d response count %d want %d", want, window, replies, expected)
				}
				if before.loaded && (s.main != before.main || s.cursor != before.cursor || s.output != before.output || s.selected != before.selected) {
					return fmt.Errorf("window notification changed owner stacks/selection")
				}
				return nil
			}
		}
	}
	return fmt.Errorf("inventory window control reply missing")
}
