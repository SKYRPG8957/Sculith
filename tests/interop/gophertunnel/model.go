package main

import (
	"encoding/json"
	"fmt"
	"github.com/sandertv/gophertunnel/minecraft"
	"github.com/sandertv/gophertunnel/minecraft/protocol/packet"
)

func sharedModel(conn *minecraft.Conn, observed *observer) error {
	observed.mu.Lock()
	registered := observed.modelRegistry
	observed.mu.Unlock()
	if !registered {
		return fmt.Errorf("custom model actor was not registered before spawn")
	}
	packs := conn.ResourcePacks()
	if len(packs) != 1 {
		return fmt.Errorf("missing model pack")
	}
	raw, err := packs[0].ReadFile("models/entity/sculith_statue.geo.json")
	if err != nil {
		return err
	}
	var geometry struct {
		Version    string `json:"format_version"`
		Geometries []struct {
			Description struct{ Identifier string }
			Bones       []struct {
				Name   string
				Parent string
				Cubes  []struct {
					Origin [3]float32
					Size   [3]float32
				}
			}
		} `json:"minecraft:geometry"`
	}
	if err := json.Unmarshal(raw, &geometry); err != nil {
		return err
	}
	if geometry.Version != "1.12.0" || len(geometry.Geometries) != 1 {
		return fmt.Errorf("invalid source geometry")
	}
	model := geometry.Geometries[0]
	if model.Description.Identifier != "geometry.sculith.statue" || len(model.Bones) != 4 || len(model.Bones[3].Cubes) != 1 || model.Bones[3].Cubes[0].Size != [3]float32{12, 8, 12} {
		return fmt.Errorf("original model geometry differs")
	}
	// The dedicated wrapper creates Java player/model 1/2, then native 3/4.
	// This relation rejects a private Java actor accidentally broadcast here.
	expected := uint64(conn.GameData().EntityRuntimeID) + 1
	phase := 0
	for i := 0; i < 256; i++ {
		pk, err := conn.ReadPacket()
		if err != nil {
			return err
		}
		switch value := pk.(type) {
		case *packet.RemoveActor:
			if (phase != 0 && phase != 2 && phase != 4) || value.EntityUniqueID != int64(expected) {
				return fmt.Errorf("unexpected model removal/isolation: phase=%d %+v", phase, value)
			}
			phase++
			if phase == 5 {
				fmt.Println("PASS: independent Bedrock registered static model, original geometry, spawn/replacement/hide and private actor IDs")
				return nil
			}
		case *packet.AddActor:
			_, passenger := value.PassengerOfBlock.Value()
			if (phase != 1 && phase != 3) || value.EntityType != "sculith:demo_statue" || value.EntityUniqueID != int64(expected) || value.EntityRuntimeID != expected ||
				value.Position[0] != .5 || value.Position[1] != 4 || value.Position[2] != 3.5 || value.Velocity[0] != 0 || value.Velocity[1] != 0 || value.Velocity[2] != 0 ||
				value.Pitch != 0 || value.Yaw != 0 || value.HeadYaw != 0 || value.BodyYaw != 0 || len(value.Attributes) != 0 || len(value.EntityMetadata) != 0 || len(value.EntityLinks) != 0 || passenger {
				return fmt.Errorf("unexpected model actor/isolation: phase=%d %+v", phase, value)
			}
			phase++
		}
	}
	return fmt.Errorf("model lifecycle incomplete")
}
