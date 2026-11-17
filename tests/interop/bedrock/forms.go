package main

import (
	"encoding/json"
	"fmt"
	"time"

	"github.com/sandertv/gophertunnel/minecraft"
	"github.com/sandertv/gophertunnel/minecraft/protocol"
	"github.com/sandertv/gophertunnel/minecraft/protocol/packet"
)

func checkForms(c *minecraft.Conn) error {
	var previous *packet.ModalFormResponse
	var lastID uint32
	write := func(id uint32, data string) error {
		if err := c.WritePacket(&packet.ModalFormResponse{FormID: id, ResponseData: protocol.Option([]byte(data))}); err != nil {
			return err
		}
		return c.Flush()
	}
	for stage := 0; stage < 4; stage++ {
		c.SetReadDeadline(time.Now().Add(15 * time.Second))
		var form *packet.ModalFormRequest
		for n := 0; n < 4096; n++ {
			pk, err := c.ReadPacket()
			if err != nil {
				return err
			}
			if request, ok := pk.(*packet.ModalFormRequest); ok {
				form = request
				break
			}
		}
		if form == nil || form.FormID < 1<<31 || form.FormID <= lastID {
			return fmt.Errorf("missing or reused shared form at stage %d: %#v", stage, form)
		}
		lastID = form.FormID
		var definition map[string]any
		if err := json.Unmarshal(form.FormData, &definition); err != nil {
			return err
		}
		expected := []string{"form", "modal", "custom_form", "form"}[stage]
		if definition["type"] != expected {
			return fmt.Errorf("form type at stage %d: %#v", stage, definition)
		}
		if previous != nil {
			if err := c.WritePacket(previous); err != nil {
				return err
			}
		}
		for _, invalid := range []string{"-1", "{}"} {
			if err := write(form.FormID, invalid); err != nil {
				return err
			}
		}
		if err := write(form.FormID+100, "0"); err != nil {
			return err
		}
		response := []string{"0", "false", "[null,true,2.5,1,0,\"가나다😀\"]", "null"}[stage]
		previous = &packet.ModalFormResponse{FormID: form.FormID, ResponseData: protocol.Option([]byte(response))}
		if err := c.WritePacket(previous); err != nil {
			return err
		}
		if err := c.Flush(); err != nil {
			return err
		}
	}
	if err := c.WritePacket(previous); err != nil {
		return err
	}
	if err := c.Flush(); err != nil {
		return err
	}
	fmt.Println("PASS: shared simple/modal/custom/cancel forms and invalid/replayed native responses")
	return nil
}
