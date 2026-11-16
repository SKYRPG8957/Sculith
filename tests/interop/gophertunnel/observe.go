package main

import (
	"bytes"
	"fmt"
	"net"
	"reflect"
	"sync"

	"github.com/df-mc/dragonfly/server/block/cube"
	"github.com/df-mc/dragonfly/server/world"
	"github.com/df-mc/dragonfly/server/world/chunk"
	"github.com/sandertv/gophertunnel/minecraft/nbt"
	"github.com/sandertv/gophertunnel/minecraft/protocol"
	"github.com/sandertv/gophertunnel/minecraft/protocol/packet"
)

// Test-only independent decoding, using a pinned full vanilla registry rather
// than copying the Rust encoder's block hashes or palette implementation.
type observer struct {
	commands                     map[string]protocol.Command
	mu                           sync.Mutex
	registry                     world.BlockRegistry
	chunks                       map[protocol.ChunkPos]bool
	biomes, creative, structures bool
	modelRegistry                bool
	auxiliary                    map[uint32]bool
	err                          error
}

func newObserver(registry world.BlockRegistry) *observer {
	return &observer{registry: registry, chunks: make(map[protocol.ChunkPos]bool), auxiliary: make(map[uint32]bool)}
}

func (o *observer) packet(header packet.Header, payload []byte, _, _ net.Addr) {
	o.mu.Lock()
	defer o.mu.Unlock()
	// Gophertunnel's codec reports malformed input through panic.
	defer func() {
		if value := recover(); value != nil && o.err == nil {
			o.err = fmt.Errorf("independent packet decoder: %v", value)
		}
	}()
	if o.err != nil {
		return
	}
	buf := bytes.NewBuffer(payload)
	reader := protocol.NewReader(buf, 0, true)
	switch header.PacketID {
	case packet.IDAvailableCommands:
		p := new(packet.AvailableCommands)
		p.Marshal(reader)
		o.commands = map[string]protocol.Command{}
		for _, command := range p.Commands {
			if _, exists := o.commands[command.Name]; exists || len(command.Overloads) != 1 || len(command.Overloads[0].Parameters) != 1 || command.AliasesOffset != ^uint32(0) || command.PermissionLevel != protocol.CommandPermissionLevelAny {
				o.err = fmt.Errorf("invalid public command registry")
				return
			}
			param := command.Overloads[0].Parameters[0]
			if param.Name != "arguments" || !param.Optional || param.Type != protocol.CommandArgValid|protocol.CommandArgTypeRawText {
				o.err = fmt.Errorf("invalid optional command argument")
				return
			}
			o.commands[command.Name] = command
		}
		if _, exists := o.commands["help"]; !exists {
			o.err = fmt.Errorf("missing help command")
		}
	case packet.IDAvailableActorIdentifiers:
		p := new(packet.AvailableActorIdentifiers)
		p.Marshal(reader)
		var value struct {
			IDs []struct {
				ID string `nbt:"id"`
			} `nbt:"idlist"`
		}
		if err := nbt.Unmarshal(p.SerialisedEntityIdentifiers, &value); err != nil {
			o.err = err
			return
		}
		for _, entry := range value.IDs {
			if entry.ID == "sculith:demo_statue" {
				o.modelRegistry = true
			}
		}

	case packet.IDJigsawStructureData:
		p := new(packet.JigsawStructureData)
		p.Marshal(reader)
		if len(p.StructureData) != 4 {
			o.err = fmt.Errorf("missing structure registries")
			return
		}
		for _, name := range []string{"processors", "template_pools", "jigsaws", "structure_sets"} {
			value := reflect.ValueOf(p.StructureData[name])
			if value.Kind() != reflect.Slice || value.Len() != 0 {
				o.err = fmt.Errorf("invalid structure registry %s", name)
				return
			}
		}
		o.structures = true
	case packet.IDStartGame:
		if !o.structures {
			o.err = fmt.Errorf("StartGame preceded mandatory structure data (native error 148)")
		}
		return
	case packet.IDBiomeDefinitionList:
		p := new(packet.BiomeDefinitionList)
		p.Marshal(reader)
		if len(p.BiomeDefinitions) != 1 || len(p.StringList) != 5 {
			o.err = fmt.Errorf("unexpected biome registry size")
			return
		}
		b := p.BiomeDefinitions[0]
		if b.NameIndex != 0 || p.StringList[0] != "plains" || b.BiomeID != -1 ||
			b.Temperature != .8 || b.Downfall != .4 || !b.Rain {
			o.err = fmt.Errorf("invalid plains definition")
			return
		}
		o.biomes = true
	case packet.IDCreativeContent:
		p := new(packet.CreativeContent)
		p.Marshal(reader)
		if len(p.Groups) != 1 || len(p.Items) != 4 {
			o.err = fmt.Errorf("unexpected creative catalog")
			return
		}
		for index, item := range p.Items {
			if item.CreativeItemNetworkID != uint32(index+1) || item.Item.NetworkID != []int32{1, 3, 2, 7}[index] || item.Item.Count != 1 || item.GroupIndex != 0 {
				o.err = fmt.Errorf("invalid creative catalogue mapping at %d: %+v", index, item)
				return
			}
		}
		o.creative = true
	case packet.IDInventoryContent:
		p := new(packet.InventoryContent)
		p.Marshal(reader)
		size, aux := map[uint32]int{120: 5, 124: 57, 119: 1}[p.WindowID]
		if !aux {
			return
		}
		_, dynamic := p.Container.DynamicContainerID.Value()
		if len(p.Content) != size || p.Container.ContainerID != 0 || dynamic || o.auxiliary[p.WindowID] {
			o.err = fmt.Errorf("invalid auxiliary inventory baseline: window=%d slots=%d", p.WindowID, len(p.Content))
			return
		}
		for _, item := range p.Content {
			if item.Stack.NetworkID != 0 || item.Stack.Count != 0 || item.StackNetworkID != 0 {
				o.err = fmt.Errorf("nonempty auxiliary inventory baseline")
				return
			}
		}
		o.auxiliary[p.WindowID] = true
	case packet.IDLevelChunk:
		p := new(packet.LevelChunk)
		p.Marshal(reader)
		if p.CacheEnabled || p.SubChunkCount != 24 || p.Dimension != 0 ||
			p.Position[0] < -2 || p.Position[0] > 2 || p.Position[1] < -2 || p.Position[1] > 2 ||
			o.chunks[p.Position] {
			o.err = fmt.Errorf("invalid or duplicate chunk %+v", p.Position)
			return
		}
		data := bytes.NewBuffer(p.RawPayload)
		c, err := chunk.NetworkDecodeBuffer(o.registry, data, int(p.SubChunkCount), cube.Range{-64, 319})
		if err != nil {
			o.err = err
			return
		}
		if data.Len() != 1 || data.Next(1)[0] != 0 {
			o.err = fmt.Errorf("invalid chunk border/block-entity suffix")
			return
		}
		for y := int16(-64); y <= 319; y++ {
			want := "minecraft:air"
			switch y {
			case 0:
				want = "minecraft:bedrock"
			case 1, 2:
				want = "minecraft:dirt"
			case 3:
				want = "minecraft:grass_block"
			}
			for x := uint8(0); x < 16; x++ {
				for z := uint8(0); z < 16; z++ {
					hash := c.Block(x, y, z, 0)
					rid, ok := o.registry.HashToRuntimeID(hash)
					if !ok {
						o.err = fmt.Errorf("unrecognized native block hash %08x at %d,%d,%d", hash, x, y, z)
						return
					}
					name, _, _ := o.registry.RuntimeIDToState(rid)
					if name != want || c.Biome(x, y, z) != 1 {
						o.err = fmt.Errorf("wrong block/biome at %d,%d,%d: %s", x, y, z, name)
						return
					}
				}
			}
		}
		o.chunks[p.Position] = true
	default:
		return
	}
	if buf.Len() != 0 {
		o.err = fmt.Errorf("trailing bytes in server packet %d", header.PacketID)
	}
}

func (o *observer) verify() error {
	o.mu.Lock()
	defer o.mu.Unlock()
	if o.err != nil {
		return o.err
	}
	if !o.structures || !o.biomes || !o.creative || len(o.chunks) != 25 || len(o.commands) == 0 || len(o.auxiliary) != 3 {
		return fmt.Errorf("incomplete startup: structures=%t biomes=%t creative=%t chunks=%d auxiliary=%d", o.structures, o.biomes, o.creative, len(o.chunks), len(o.auxiliary))
	}
	return nil
}
