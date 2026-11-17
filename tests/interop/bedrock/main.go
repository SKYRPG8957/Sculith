package main

import (
	"context"
	"fmt"
	"github.com/df-mc/dragonfly/server/world"
	"github.com/df-mc/go-nethernet"
	"github.com/go-gl/mathgl/mgl32"
	"github.com/pion/webrtc/v4"
	"github.com/sandertv/gophertunnel/minecraft"
	"github.com/sandertv/gophertunnel/minecraft/protocol"
	"github.com/sandertv/gophertunnel/minecraft/protocol/packet"
	"log/slog"
	"os"
	"time"
)

func run() error {
	var setting webrtc.SettingEngine
	setting.SetIncludeLoopbackCandidate(true)
	setting.SetNetworkTypes([]webrtc.NetworkType{webrtc.NetworkTypeUDP4})
	api := webrtc.NewAPI(webrtc.WithSettingEngine(setting))
	minecraft.RegisterNetwork("local-evaluation", func(log *slog.Logger) minecraft.Network {
		return minecraft.NetherNet{Log: log, Dialer: nethernet.Dialer{API: api}}
	})
	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
	defer cancel()
	d := minecraft.Dialer{}
	d.IdentityData.DisplayName = "ForkProbe"
	c, err := d.DialContext(ctx, "local-evaluation", os.Args[1])
	if err != nil {
		return fmt.Errorf("dial: %w", err)
	}
	defer c.Close()
	c.SetReadDeadline(time.Now().Add(15 * time.Second))
	if err = c.DoSpawn(); err != nil {
		return fmt.Errorf("spawn: %w", err)
	}
	g := c.GameData()
	fmt.Printf("SPAWN id=%d position=%v hashed=%v\n", g.EntityRuntimeID, g.PlayerPosition, g.UseBlockNetworkIDHashes)
	time.Sleep(1200 * time.Millisecond)
	registry := world.NewBlockRegistry()
	registry.Finalize()
	name := func(id uint32) string {
		if g.UseBlockNetworkIDHashes {
			var ok bool
			id, ok = registry.HashToRuntimeID(id)
			if !ok {
				return fmt.Sprintf("unknown-hash:%d", id)
			}
		}
		n, _, ok := registry.RuntimeIDToState(id)
		if !ok {
			return fmt.Sprintf("unknown-id:%d", id)
		}
		return n
	}
	write := func(p packet.Packet) error {
		if e := c.WritePacket(p); e != nil {
			return e
		}
		return c.Flush()
	}
	wait := func(stage string, accept func(packet.Packet) bool) error {
		c.SetReadDeadline(time.Now().Add(6 * time.Second))
		for n := 0; n < 4096; n++ {
			p, e := c.ReadPacket()
			if e != nil {
				return fmt.Errorf("%s: %w", stage, e)
			}
			if n < 14 {
				fmt.Printf("OBSERVE %s %T\n", stage, p)
			}
			if accept(p) {
				return nil
			}
		}
		return fmt.Errorf("%s exceeded packet budget", stage)
	}
	pick := protocol.BlockPos{2, -60, 2}
	if len(os.Args) > 2 && os.Args[2] == "--read-persisted" {
		pick = protocol.BlockPos{3, -60, 2}
	}
	if err = write(&packet.BlockPickRequest{Position: pick, HotBarSlot: 0}); err != nil {
		return err
	}
	var held protocol.ItemInstance
	err = wait("pick Java diamond marker", func(p packet.Packet) bool {
		switch v := p.(type) {
		case *packet.InventoryContent:
			if v.WindowID == 0 && len(v.Content) > 0 && v.Content[0].Stack.Count > 0 {
				held = v.Content[0]
				return true
			}
		case *packet.InventorySlot:
			if v.WindowID == 0 && v.Slot == 0 && v.NewItem.Stack.Count > 0 {
				held = v.NewItem
				return true
			}
		}
		return false
	})
	if err != nil {
		return err
	}
	var itemName string
	for _, v := range g.Items {
		if int32(v.RuntimeID) == held.Stack.NetworkID {
			itemName = v.Name
		}
	}
	fmt.Printf("PICK item=%s id=%d count=%d stack=%d\n", itemName, held.Stack.NetworkID, held.Stack.Count, held.StackNetworkID)
	if itemName != "minecraft:diamond_block" {
		return fmt.Errorf("marker item mismatch: %s", itemName)
	}
	if len(os.Args) > 2 && os.Args[2] == "--read-persisted" {
		fmt.Println("PASS: native client read server-saved diamond marker", pick)
		return nil
	}
	for index, mode := range []string{"standalone", "embedded", "predicted-held", "embedded-journal"} {
		target := protocol.BlockPos{2 + int32(index%2), -60, 3 + int32(index/2)}
		base := protocol.BlockPos{target[0], -61, target[2]}
		data := protocol.UseItemTransactionData{ActionType: protocol.UseItemActionClickBlock, BlockPosition: base, BlockFace: 1, HotBarSlot: 0, HeldItem: held, Position: g.PlayerPosition, ClickedPosition: mgl32.Vec3{.5, 1, .5}}
		if mode == "predicted-held" {
			data.HeldItem.StackNetworkID = -7
		}
		if mode == "embedded-journal" {
			data.Actions = []protocol.InventoryAction{{SourceType: protocol.InventoryActionSourceContainer, WindowID: protocol.Option(int8(0)), InventorySlot: 0, OldItem: held, NewItem: held}}
		}
		var request packet.Packet = &packet.InventoryTransaction{TransactionData: &data}
		if mode == "embedded" || mode == "embedded-journal" {
			request = &packet.PlayerAuthInput{Position: g.PlayerPosition, InputMode: packet.InputModeMouse, PlayMode: packet.PlayModeScreen, InteractionModel: packet.InteractionModelCrosshair, Tick: uint64(index + 1), InputData: protocol.NewInputFlagsFromIDs(packet.InputFlagCount, []int32{packet.InputFlagPerformItemInteraction}), ItemInteractionData: protocol.Option(data), CameraOrientation: mgl32.Vec3{0, 0, 1}}
		}
		if err = write(request); err != nil {
			return err
		}
		if err = wait(mode+" place", func(p packet.Packet) bool {
			v, ok := p.(*packet.UpdateBlock)
			return ok && v.Position == target && name(v.NewBlockRuntimeID) == "minecraft:diamond_block"
		}); err != nil {
			return err
		}
		fmt.Println("PLACED", mode, target)
		for _, a := range []int32{protocol.PlayerActionStartBreak, protocol.PlayerActionPredictDestroyBlock} {
			if err = write(&packet.PlayerAction{EntityRuntimeID: g.EntityRuntimeID, ActionType: a, BlockPosition: target, ResultPosition: target, BlockFace: 1}); err != nil {
				return err
			}
		}
		if err = wait(mode+" break", func(p packet.Packet) bool {
			v, ok := p.(*packet.UpdateBlock)
			return ok && v.Position == target && name(v.NewBlockRuntimeID) == "minecraft:air"
		}); err != nil {
			return err
		}
		fmt.Println("BROKE", mode, target)
		time.Sleep(150 * time.Millisecond)
	}
	target := protocol.BlockPos{3, -60, 2}
	if err = write(&packet.InventoryTransaction{TransactionData: &protocol.UseItemTransactionData{ActionType: protocol.UseItemActionClickBlock, BlockPosition: protocol.BlockPos{3, -61, 2}, BlockFace: 1, HotBarSlot: 0, HeldItem: held, Position: g.PlayerPosition, ClickedPosition: mgl32.Vec3{.5, 1, .5}}}); err != nil {
		return err
	}
	if err = wait("persistent marker place", func(p packet.Packet) bool {
		v, ok := p.(*packet.UpdateBlock)
		return ok && v.Position == target && name(v.NewBlockRuntimeID) == "minecraft:diamond_block"
	}); err != nil {
		return err
	}
	fmt.Println("PASS: imported Java marker, four native building paths, new persistence marker", target)
	return nil
}
func main() {
	if err := run(); err != nil {
		fmt.Fprintln(os.Stderr, "FAIL:", err)
		os.Exit(1)
	}
}
