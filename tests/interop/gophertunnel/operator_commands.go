package main

import (
	"fmt"
	"github.com/google/uuid"
	"github.com/sandertv/gophertunnel/minecraft"
	"github.com/sandertv/gophertunnel/minecraft/protocol"
	"github.com/sandertv/gophertunnel/minecraft/protocol/packet"
)

func operatorCommands(conn *minecraft.Conn, observed *observer) error {
	observed.mu.Lock()
	count := len(observed.commands)
	for _, name := range []string{"help", "dual_probe", "legacy_probe", "observer_probe"} {
		if _, ok := observed.commands[name]; !ok {
			observed.mu.Unlock()
			return fmt.Errorf("missing player command %s", name)
		}
	}
	observed.mu.Unlock()
	if count != 4 {
		return fmt.Errorf("console-only command advertised to a player")
	}
	for _, request := range []struct {
		line   string
		origin uint32
	}{
		{"/console_probe stolen", protocol.CommandOriginDedicatedServer},
		{"/console_probe stolen", protocol.CommandOriginPlayer},
		{"/dual_probe native", protocol.CommandOriginPlayer},
	} {
		if err := conn.WritePacket(&packet.CommandRequest{CommandLine: request.line, CommandOrigin: protocol.CommandOrigin{Origin: request.origin, UUID: uuid.New()}}); err != nil {
			return err
		}
	}
	if err := conn.Flush(); err != nil {
		return err
	}
	rejected := false
	for i := 0; i < 256; i++ {
		pk, err := conn.ReadPacket()
		if err != nil {
			return err
		}
		if text, ok := pk.(*packet.Text); ok {
			if text.Message == "This command is not available from this source." {
				rejected = true
				continue
			}
			if text.Message == "command acknowledged" && rejected {
				fmt.Println("PASS: native command catalogs, console-origin rejection and private player acknowledgement")
				return nil
			}
			return fmt.Errorf("unexpected command reply: %q", text.Message)
		}
	}
	return fmt.Errorf("missing player command acknowledgement")
}
