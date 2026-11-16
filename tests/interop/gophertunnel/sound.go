package main

import (
	"crypto/sha256"
	"encoding/json"
	"fmt"
	"github.com/sandertv/gophertunnel/minecraft"
	"github.com/sandertv/gophertunnel/minecraft/protocol/packet"
)

func sharedSound(conn *minecraft.Conn) error {
	packs := conn.ResourcePacks()
	if len(packs) != 1 {
		return fmt.Errorf("missing native sound pack")
	}
	clip, err := packs[0].ReadFile("sounds/sculith_demo.ogg")
	if err != nil {
		return err
	}
	if fmt.Sprintf("%x", sha256.Sum256(clip)) != "8e15745a94e04437cb78f5ae39fb2a6dda7390baae40079f91c2015effd8001f" {
		return fmt.Errorf("native clip bytes differ")
	}
	raw, err := packs[0].ReadFile("sounds/sound_definitions.json")
	if err != nil {
		return err
	}
	var definitions struct {
		Definitions map[string]struct {
			Category string
			Sounds   []string
		} `json:"sound_definitions"`
	}
	if err := json.Unmarshal(raw, &definitions); err != nil {
		return err
	}
	definition := definitions.Definitions["sculith.demo_tone"]
	if definition.Category != "player" || len(definition.Sounds) != 1 || definition.Sounds[0] != "sounds/sculith_demo" {
		return fmt.Errorf("native sound definition differs")
	}
	for i := 0; i < 128; i++ {
		pk, err := conn.ReadPacket()
		if err != nil {
			return err
		}
		sound, ok := pk.(*packet.PlaySound)
		if !ok {
			continue
		}
		_, handle := sound.Handle.Value()
		_, offset := sound.PlaybackPositionSeconds.Value()
		if sound.SoundName != "sculith.demo_tone" || sound.Position[0] != .5 || sound.Position[1] != 5.5 || sound.Position[2] != .5 || sound.Volume != .7 || sound.Pitch != 1 || sound.LoopCount != -1 || sound.BypassListenerRangeCheck || handle || offset {
			return fmt.Errorf("unexpected private sound: %+v", sound)
		}
		fmt.Println("PASS: independent Bedrock original OGG, sound definition and private non-looping PlaySound")
		return nil
	}
	return fmt.Errorf("missing native sound packet")
}
