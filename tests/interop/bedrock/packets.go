package main

import (
	"fmt"
	"time"

	"github.com/sandertv/gophertunnel/minecraft"
	"github.com/sandertv/gophertunnel/minecraft/protocol"
	"github.com/sandertv/gophertunnel/minecraft/protocol/packet"
)

// The wire schema stays current. Only the proposal is changed to test admission after a hook.
type proposedProtocol struct{ minecraft.Protocol }

func (proposedProtocol) ID() int32 { return protocol.CurrentProtocol - 1 }

func checkPacketEdits(c *minecraft.Conn) error {
	expected := []string{"sculith:rewritten", "sculith:payload", "sculith:sentinel"}
	c.SetReadDeadline(time.Now().Add(10 * time.Second))
	for n := 0; n < 4096; n++ {
		pk, err := c.ReadPacket()
		if err != nil {
			return err
		}
		switch v := pk.(type) {
		case *packet.PlaySound:
			return fmt.Errorf("original sound packet escaped the plugin: %#v", v)
		case *packet.StopSound:
			if v.StopAll || v.StopMusicLegacy || v.SoundName != expected[0] {
				return fmt.Errorf("packet edit expected %q, got %#v", expected[0], v)
			}
			expected = expected[1:]
			if len(expected) == 0 {
				fmt.Println("PASS: packet ID/payload edits, cancellation and unchanged sentinel")
				return nil
			}
		}
	}
	return fmt.Errorf("missing rewritten packet sequence: %v", expected)
}
