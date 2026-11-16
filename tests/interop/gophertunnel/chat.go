package main

import (
	"fmt"
	"github.com/google/uuid"
	"github.com/sandertv/gophertunnel/minecraft"
	"github.com/sandertv/gophertunnel/minecraft/protocol"
	"github.com/sandertv/gophertunnel/minecraft/protocol/packet"
	"time"
)

// Hold an independent native session while a real, unmodified Java client
// sends public chat and invokes the Rust /hello command through its own UI.
func sharedChatVisual(conn *minecraft.Conn) error {
	if err := conn.WritePacket(&packet.Text{TextType: packet.TextTypeChat, SourceName: "ForgedAuthor", Message: "native 안녕 😀"}); err != nil {
		return err
	}
	if err := conn.WritePacket(&packet.CommandRequest{CommandLine: "/hello native", CommandOrigin: protocol.CommandOrigin{Origin: protocol.CommandOriginPlayer, UUID: uuid.New()}}); err != nil {
		return err
	}
	if err := conn.Flush(); err != nil {
		return err
	}
	end := time.Now().Add(35 * time.Second)
	if err := conn.SetReadDeadline(end); err != nil {
		return err
	}
	seen := 0
	fmt.Println("CHAT_VISUAL_READY")
	for {
		pk, err := conn.ReadPacket()
		if err != nil {
			if !time.Now().Before(end) && seen == 7 {
				fmt.Println("PASS: actual stock Java public chat, private Rust command and bidirectional native chat")
				return nil
			}
			return fmt.Errorf("chat visual incomplete (%d): %w", seen, err)
		}
		if text, ok := pk.(*packet.Text); ok {
			fmt.Printf("CHAT_VISUAL: type=%d author=%s message=%s\n", text.TextType, text.SourceName, text.Message)
			if text.TextType == packet.TextTypeChat && text.SourceName == "베드락😀" && text.Message == "native 안녕 😀" {
				seen |= 1
			}
			if text.TextType == packet.TextTypeSystem && text.Message == "Hello 베드락😀 👋" {
				seen |= 2
			}
			if text.TextType == packet.TextTypeChat && text.SourceName == "OfficialProbe" && text.Message == "stock Java chat online" {
				seen |= 4
			}
			if text.Message == "Hello OfficialProbe 👋" {
				return fmt.Errorf("foreign private command reply leaked")
			}
		}
	}
}
