package main

import (
	"context"
	"fmt"
	"github.com/sandertv/gophertunnel/minecraft"
	"os"
	"time"

	"github.com/df-mc/dragonfly/server/world"
)

func main() {
	if len(os.Args) != 2 {
		fmt.Fprintln(os.Stderr, "usage: gophertunnel-probe http://127.0.0.1:port")
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
}
