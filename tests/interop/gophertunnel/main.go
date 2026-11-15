package main

import (
	"context"
	"fmt"
	"os"
	"time"

	"github.com/df-mc/dragonfly/server/world"
	"github.com/sandertv/gophertunnel/minecraft"
)

func main() {
	if len(os.Args) != 2 && !(len(os.Args) == 3 && os.Args[2] == "--crossplay") {
		fmt.Fprintln(os.Stderr, "usage: gophertunnel-probe http://127.0.0.1:port [--crossplay]")
		os.Exit(2)
	}
	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
	defer cancel()
	registry := world.NewBlockRegistry()
	registry.Finalize()
	observed := newObserver(registry)
	conn, err := (minecraft.Dialer{PacketFunc: observed.packet}).DialContext(ctx, "nethernet", os.Args[1])
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
	if len(os.Args) == 3 {
		if err := crossplay(conn, registry); err != nil {
			panic(err)
		}
	}
}
