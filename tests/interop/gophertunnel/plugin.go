package main

import (
	"fmt"
	"github.com/sandertv/gophertunnel/minecraft"
	"github.com/sandertv/gophertunnel/minecraft/protocol/packet"
	"strings"
)

func pluginMessage(conn *minecraft.Conn) error {
	for i := 0; i < 128; i++ {
		pk, err := conn.ReadPacket()
		if err != nil {
			return err
		}
		if message, ok := pk.(*packet.Text); ok {
			if message.TextType != packet.TextTypeSystem || message.NeedsTranslation ||
				!strings.HasPrefix(message.Message, "Sculith: ") ||
				!strings.HasSuffix(message.Message, "님, 환영합니다 👋") || message.XUID != "" || message.PlatformChatID != "" {
				return fmt.Errorf("invalid plugin system message: %+v", message)
			}
			fmt.Println("PASS: independent Bedrock decoder received Rust plugin's private Unicode system message")
			return nil
		}
	}
	return fmt.Errorf("plugin system message missing")
}
