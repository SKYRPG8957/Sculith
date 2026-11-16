package main

import (
	"context"
	"fmt"
	"os"
	"path/filepath"
	"time"

	"github.com/df-mc/dragonfly/server/world"
	"github.com/sandertv/gophertunnel/minecraft"
)

func main() {
	packOutput := len(os.Args) == 4 && os.Args[2] == "--pack-output"
	modelOutput := len(os.Args) == 4 && os.Args[2] == "--model-output"
	if len(os.Args) != 2 && !(len(os.Args) == 3 && (os.Args[2] == "--model" || os.Args[2] == "--sound" || os.Args[2] == "--crossplay" || os.Args[2] == "--plugin-message" || os.Args[2] == "--form" || os.Args[2] == "--form-cancel")) && !packOutput && !modelOutput {
		fmt.Fprintln(os.Stderr, "usage: gophertunnel-probe http://127.0.0.1:port [--model|--sound|--crossplay|--plugin-message|--form|--form-cancel|--pack-output file|--model-output file]")
		os.Exit(2)
	}
	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
	defer cancel()
	registry := world.NewBlockRegistry()
	registry.Finalize()
	observed := newObserver(registry)
	dialer := minecraft.Dialer{PacketFunc: observed.packet}
	if packOutput {
		dialer.ResourcePackCache = minecraft.DirResourcePackCache{Dir: filepath.Join(filepath.Dir(os.Args[3]), "cache")}
	}
	conn, err := dialer.DialContext(ctx, "nethernet", os.Args[1])
	if err != nil {
		panic(fmt.Errorf("dial: %w", err))
	}
	defer conn.Close()
	fmt.Println("DIALED: independent gophertunnel Bedrock login completed")
	_ = conn.SetReadDeadline(time.Now().Add(15 * time.Second))
	if err := conn.DoSpawn(); err != nil {
		panic(fmt.Errorf("spawn: %w", err))
	}
	if err := observed.verify(); err != nil {
		panic(err)
	}
	fmt.Println("PASS: independent Dragonfly decoding of 25 chunks, all 2457600 blocks, plains biomes and startup definitions")
	fmt.Println("PASS: independent gophertunnel NetherNet world spawn", conn.GameData().EntityRuntimeID)
	if packOutput || modelOutput {
		packs := conn.ResourcePacks()
		if len(packs) != 1 {
			panic(fmt.Errorf("expected exactly one received resource pack, got %d", len(packs)))
		}
		data := make([]byte, packs[0].Len())
		if n, err := packs[0].ReadAt(data, 0); err != nil || n != len(data) {
			panic(fmt.Errorf("read downloaded pack: %d %v", n, err))
		}
		if err := os.WriteFile(os.Args[3], data, 0600); err != nil {
			panic(err)
		}
		fmt.Println("PASS: received Bedrock resource pack", packs[0].UUID(), packs[0].Version(), len(data))
	}
	if modelOutput {
		if err := sharedModel(conn, observed); err != nil {
			panic(err)
		}
	}
	if len(os.Args) == 3 {
		var err error
		if os.Args[2] == "--model" {
			err = sharedModel(conn, observed)
		} else if os.Args[2] == "--sound" {
			err = sharedSound(conn)
		} else if os.Args[2] == "--crossplay" {
			err = crossplay(conn, registry)
		} else if os.Args[2] == "--form" || os.Args[2] == "--form-cancel" {
			err = formResponse(conn, os.Args[2] == "--form-cancel")
		} else {
			err = pluginMessage(conn)
		}
		if err != nil {
			panic(err)
		}
	}
}
