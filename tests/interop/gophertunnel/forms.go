package main

import (
	"encoding/json"
	"fmt"
	"github.com/sandertv/gophertunnel/minecraft"
	"github.com/sandertv/gophertunnel/minecraft/protocol"
	"github.com/sandertv/gophertunnel/minecraft/protocol/packet"
	"strings"
)

func formResponse(conn *minecraft.Conn, cancelled bool) error {
	var formID uint32
	for i := 0; i < 128; i++ {
		pk, err := conn.ReadPacket()
		if err != nil {
			return err
		}
		request, ok := pk.(*packet.ModalFormRequest)
		if !ok {
			continue
		}
		var form struct {
			Type    string
			Title   string
			Content string
			Buttons []struct{ Text string }
		}
		if err := json.Unmarshal(request.FormData, &form); err != nil {
			return err
		}
		if form.Type != "form" || form.Title != "메뉴" ||
			form.Content != "Java와 베드락에서 같은 메뉴를 사용합니다.\n원하는 항목을 선택하세요." ||
			len(form.Buttons) != 2 || form.Buttons[0].Text != "기능 안내" || form.Buttons[1].Text != "닫기" {
			return fmt.Errorf("unexpected shared form: %+v", form)
		}
		formID = request.FormID
		break
	}
	if formID == 0 {
		return fmt.Errorf("shared form missing")
	}
	// Another session's preceding form must remain pending. A malformed choice
	// must also leave this session's own form available for its valid response.
	for _, reply := range []*packet.ModalFormResponse{
		{FormID: formID - 1, ResponseData: protocol.Option([]byte("0"))},
		{FormID: formID, ResponseData: protocol.Option([]byte("3.5"))},
	} {
		if err := conn.WritePacket(reply); err != nil {
			return err
		}
	}
	response := &packet.ModalFormResponse{FormID: formID, ResponseData: protocol.Option([]byte("0\n"))}
	if cancelled {
		response = &packet.ModalFormResponse{FormID: formID, CancelReason: protocol.Option(uint8(packet.ModalFormCancelReasonUserClosed))}
	}
	if err := conn.WritePacket(response); err != nil {
		return err
	}
	if err := conn.Flush(); err != nil {
		return err
	}
	for i := 0; i < 128; i++ {
		pk, err := conn.ReadPacket()
		if err != nil {
			return err
		}
		message, ok := pk.(*packet.Text)
		if !ok {
			continue
		}
		expected := "Sculith: 블록을 설치하거나 부수고, Ctrl+C로 서버를 종료하면 월드를 저장합니다."
		if cancelled {
			expected = "Sculith: 메뉴를 닫았습니다."
		}
		if message.TextType != packet.TextTypeSystem || message.NeedsTranslation || message.Message != expected ||
			message.XUID != "" || message.PlatformChatID != "" || !strings.HasPrefix(message.Message, "Sculith: ") {
			return fmt.Errorf("unexpected form callback message: %+v", message)
		}
		// A replay cannot generate a second callback. The wrapper checks exact
		// callback counts and recipients across both editions and the observer.
		if err := conn.WritePacket(response); err != nil {
			return err
		}
		if err := conn.Flush(); err != nil {
			return err
		}
		fmt.Println("PASS: independent Bedrock shared form and Rust callback; cancelled=", cancelled)
		return nil
	}
	return fmt.Errorf("form callback message missing")
}
