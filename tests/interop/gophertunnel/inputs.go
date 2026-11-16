package main

import (
	"encoding/json"
	"fmt"
	"reflect"
	"strings"

	"github.com/sandertv/gophertunnel/minecraft"
	"github.com/sandertv/gophertunnel/minecraft/protocol"
	"github.com/sandertv/gophertunnel/minecraft/protocol/packet"
)

func inputResponse(conn *minecraft.Conn, cancelled bool) error {
	var id uint32
	for i := 0; i < 128; i++ {
		pk, err := conn.ReadPacket()
		if err != nil {
			return err
		}
		request, ok := pk.(*packet.ModalFormRequest)
		if !ok {
			continue
		}
		var got, want any
		expected := `{"type":"custom_form","title":"공통 입력 폼 👋","content":[{"type":"input","text":"이름","default":"테스트 👋","placeholder":""},{"type":"toggle","text":"알림 받기","default":true},{"type":"dropdown","text":"표시 방식","options":["기본","확장"],"default":0},{"type":"slider","text":"크기","min":-10,"max":10,"step":5,"default":0}]}`
		if err = json.Unmarshal(request.FormData, &got); err != nil {
			return err
		}
		if err = json.Unmarshal([]byte(expected), &want); err != nil {
			return err
		}
		if !reflect.DeepEqual(got, want) {
			return fmt.Errorf("unexpected native input form: %s", request.FormData)
		}
		id = request.FormID
		break
	}
	if id == 0 {
		return fmt.Errorf("input form missing")
	}
	if err := conn.WritePacket(&packet.ModalFormResponse{FormID: id - 1, ResponseData: protocol.Option([]byte(`["forged",false,1,5]`))}); err != nil {
		return err
	}
	for _, value := range []string{`[0,false,1,5]`, `["text"]`, `["text",false,9,5]`, `["text",false,1,3]`, `["text",false,1,5.5]`, `[null,false,1,5]`, `["` + strings.Repeat("👋", 9) + `",false,1,5]`} {
		if err := conn.WritePacket(&packet.ModalFormResponse{FormID: id, ResponseData: protocol.Option([]byte(value))}); err != nil {
			return err
		}
	}
	reply := &packet.ModalFormResponse{FormID: id, ResponseData: protocol.Option([]byte(`["이름 👋",false,1,5.0]`))}
	if cancelled {
		reply = &packet.ModalFormResponse{FormID: id, CancelReason: protocol.Option(uint8(packet.ModalFormCancelReasonUserClosed))}
	}
	if err := conn.WritePacket(reply); err != nil {
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
		text, ok := pk.(*packet.Text)
		if !ok {
			continue
		}
		expected := "입력: 이름 👋 | 알림: false | 선택: 1 | 값: 5"
		if cancelled {
			expected = "입력을 취소했습니다."
		}
		if text.Message != expected || text.TextType != packet.TextTypeSystem || text.NeedsTranslation {
			return fmt.Errorf("unexpected input callback: %+v", text)
		}
		if err := conn.WritePacket(reply); err != nil {
			return err
		}
		if err := conn.Flush(); err != nil {
			return err
		}
		fmt.Println("PASS: native typed input form and private Rust callback; cancelled=", cancelled)
		return nil
	}
	return fmt.Errorf("input callback missing")
}
