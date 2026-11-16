package main

import (
	"bytes"
	"encoding/json"
	"fmt"
	"math"
	"time"

	"github.com/go-gl/mathgl/mgl32"
	"github.com/google/uuid"
	"github.com/sandertv/gophertunnel/minecraft"
	"github.com/sandertv/gophertunnel/minecraft/protocol"
	"github.com/sandertv/gophertunnel/minecraft/protocol/packet"
)

// Independent native decoding, including the current player-list/skin format.
// This is not an official Bedrock rendering or account authentication test.
func sharedPeers(conn *minecraft.Conn, visual, equipmentExercise bool) error {
	done := make(chan struct{})
	defer close(done)
	if err := conn.SetReadDeadline(time.Now().Add(55 * time.Second)); err != nil {
		return err
	}
	listed := map[uuid.UUID]protocol.PlayerListEntry{}
	entities := map[uint64]uuid.UUID{}
	ready, javaMoved, lateJoined, lateRemoved := false, false, false, false
	var late uuid.UUID
	var visualEnd time.Time
	for {
		pk, err := conn.ReadPacket()
		if err != nil {
			if visual && !visualEnd.IsZero() && !time.Now().Before(visualEnd) {
				fmt.Println("PASS: native peer held for actual stock Java rendering")
				return nil
			}
			return err
		}
		switch p := pk.(type) {
		case *packet.PlayerList:
			for _, entry := range p.Entries {
				if entry.ActionType == protocol.PlayerListActionRemove {
					if _, ok := listed[entry.UUID]; !ok {
						return fmt.Errorf("unknown peer removal")
					}
					for _, id := range entities {
						if id == entry.UUID {
							return fmt.Errorf("roster removed before actor")
						}
					}
					delete(listed, entry.UUID)
					if lateRemoved && entry.UUID == late {
						fmt.Println("PASS: independent Bedrock peer roster, skin, spawn, bidirectional movement, late join and ordered leave")
						return nil
					}
					continue
				}
				skin := entry.Skin
				trusted, present := skin.Trusted.Value()
				if entry.ActionType != protocol.PlayerListActionAdd || entry.EntityUniqueID <= 0 || entry.XUID != "" || entry.PlayFabID != "" || skin.SkinImageWidth != 64 || skin.SkinImageHeight != 64 || !bytes.Equal(skin.SkinData, bytes.Repeat([]byte{75, 145, 85, 255}, 4096)) || !present || !trusted || !json.Valid(skin.SkinGeometry) || !bytes.Contains(skin.SkinGeometry, []byte("geometry.sculith.preview_player")) {
					return fmt.Errorf("invalid peer profile or original placeholder skin: %s", entry.Username)
				}
				if _, ok := listed[entry.UUID]; ok {
					return fmt.Errorf("duplicate peer list entry")
				}
				listed[entry.UUID] = entry
			}
		case *packet.AddPlayer:
			entry, ok := listed[p.UUID]
			if !ok || entry.Username != p.Username || uint64(entry.EntityUniqueID) != p.EntityRuntimeID || p.EntityRuntimeID == conn.GameData().EntityRuntimeID || p.GameType != 1 || p.HeldItem.Stack.Count != 64 || p.HeldItem.Stack.NetworkID != 1 || math.Abs(float64(p.Position[1])-5.62) > .00001 || p.AbilityData.EntityUniqueID != entry.EntityUniqueID || len(p.AbilityData.Layers) != 1 || p.AbilityData.Layers[0].Type != protocol.AbilityLayerTypeBase {
				return fmt.Errorf("invalid native peer spawn: %+v", p)
			}
			if _, ok := entities[p.EntityRuntimeID]; ok {
				return fmt.Errorf("duplicate native peer actor")
			}
			entities[p.EntityRuntimeID] = p.UUID
			if !ready {
				if p.Username != "PeerJavaProbe" && !(visual && p.Username == "OfficialProbe") {
					return fmt.Errorf("unexpected initial peer %s", p.Username)
				}
				ready = true
				if err := conn.WritePacket(&packet.PlayerAuthInput{Position: mgl32.Vec3{.5, 5.62, 2}, Yaw: 180, HeadYaw: 180, InputMode: packet.InputModeMouse, PlayMode: packet.PlayModeScreen, InteractionModel: packet.InteractionModelCrosshair, Tick: 1, Delta: mgl32.Vec3{0, 0, 1.5}, CameraOrientation: mgl32.Vec3{0, 0, 1}}); err != nil {
					return err
				}
				if err := conn.Flush(); err != nil {
					return err
				}
				fmt.Println("PEER_READY")
				if visual {
					time.Sleep(time.Second)
					if err := conn.WritePacket(&packet.PlayerAuthInput{Position: mgl32.Vec3{.5, 5.62, 3.5}, Yaw: 180, HeadYaw: 180, InputMode: packet.InputModeMouse, InteractionModel: packet.InteractionModelCrosshair, Tick: 2, Delta: mgl32.Vec3{0, 0, 1.5}, CameraOrientation: mgl32.Vec3{0, 0, 1}}); err != nil {
						return err
					}
					if err := conn.Flush(); err != nil {
						return err
					}
					fmt.Println("PEER_VISUAL_READY")
					if equipmentExercise {
						go func() {
							for _, slot := range []uint32{1, 8} {
								select {
								case <-done:
									return
								case <-time.After(7 * time.Second):
								}
								if err := conn.WritePacket(&packet.PlayerHotBar{SelectedHotBarSlot: slot, SelectHotBarSlot: true}); err != nil {
									return
								}
								if err := conn.Flush(); err != nil {
									return
								}
							}
						}()
					}

					visualEnd = time.Now().Add(30 * time.Second)
					if err := conn.SetReadDeadline(visualEnd); err != nil {
						return err
					}
				}
			} else if p.Username == "PeerLateProbe" {
				if !javaMoved || lateJoined {
					return fmt.Errorf("invalid late join ordering")
				}
				lateJoined = true
				late = p.UUID
				fmt.Println("PEER_LATE_JOINED")
			} else {
				return fmt.Errorf("unexpected peer %s", p.Username)
			}

		case *packet.MobEquipment:
			if visual {
				fmt.Printf("PEER_ACTUAL_EQUIPMENT: id=%d item=%d count=%d slot=%d\n", p.EntityRuntimeID, p.NewItem.Stack.NetworkID, p.NewItem.Stack.Count, p.HotBarSlot)
			}
		case *packet.MovePlayer:
			if visual {
				fmt.Printf("PEER_ACTUAL_JAVA_MOVED: id=%d position=%v yaw=%v\n", p.EntityRuntimeID, p.Position, p.Yaw)
			}
			if _, ok := entities[p.EntityRuntimeID]; !ok {
				return fmt.Errorf("movement without peer")
			}
			if p.EntityRuntimeID == 1 && p.Position == (mgl32.Vec3{1.5, 5.62, .5}) && p.Yaw == 90 && p.HeadYaw == 90 && !javaMoved {
				javaMoved = true
				fmt.Println("PEER_JAVA_MOVED")
			}
		case *packet.RemoveActor:
			id, ok := entities[uint64(p.EntityUniqueID)]
			if !ok {
				return fmt.Errorf("unknown removed native peer")
			}
			delete(entities, uint64(p.EntityUniqueID))
			if id == late && lateJoined {
				lateRemoved = true
			} else {
				return fmt.Errorf("unexpected peer actor removal")
			}
		}
	}
}
