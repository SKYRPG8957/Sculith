package main

import (
	"encoding/json"
	"github.com/sandertv/gophertunnel/minecraft/protocol/packet"
	"os"
	"reflect"
	"sort"
)

type catalogEntry struct {
	ID     uint32 `json:"id"`
	Name   string `json:"name"`
	Client bool   `json:"client_pool"`
	Server bool   `json:"server_pool"`
}

// Independent live pool reflection, not a copy of the Rust registry fixture.
func packetCatalog() error {
	entries := map[uint32]*catalogEntry{}
	for _, client := range []bool{true, false} {
		pool := packet.NewServerPool()
		if client {
			pool = packet.NewClientPool()
		}
		for id, makePacket := range pool {
			row := entries[id]
			if row == nil {
				row = &catalogEntry{ID: id, Name: reflect.TypeOf(makePacket()).Elem().Name()}
				entries[id] = row
			}
			if client {
				row.Client = true
			} else {
				row.Server = true
			}
		}
	}
	list := make([]*catalogEntry, 0, len(entries))
	for _, entry := range entries {
		list = append(list, entry)
	}
	sort.Slice(list, func(i, j int) bool { return list[i].ID < list[j].ID })
	return json.NewEncoder(os.Stdout).Encode(list)
}
