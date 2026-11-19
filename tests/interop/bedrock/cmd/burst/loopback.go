package main

import (
	"fmt"
	"net"

	"github.com/pion/transport/v5"
	"github.com/pion/transport/v5/stdnet"
	"github.com/pion/webrtc/v4"
)

// The test owns its loopback address. Use ordinary OS sockets, but only offer
// this adapter to ICE so a NIC, VPN or route-table scan cannot affect the run.
type loopbackNet struct {
	stdnet.Net
	adapter *transport.Interface
}

func (n *loopbackNet) Interfaces() ([]*transport.Interface, error) {
	return []*transport.Interface{n.adapter}, nil
}

func (n *loopbackNet) InterfaceByIndex(index int) (*transport.Interface, error) {
	if index == n.adapter.Index {
		return n.adapter, nil
	}
	return nil, fmt.Errorf("unknown loopback interface index %d", index)
}

func (n *loopbackNet) InterfaceByName(name string) (*transport.Interface, error) {
	if name == n.adapter.Name {
		return n.adapter, nil
	}
	return nil, fmt.Errorf("unknown loopback interface %q", name)
}

func loopbackSettings(ip net.IP) webrtc.SettingEngine {
	adapter := transport.NewInterface(net.Interface{
		Index: 1, Name: "loopback", Flags: net.FlagUp | net.FlagLoopback, MTU: 65536,
	})
	adapter.AddAddress(&net.IPNet{IP: ip.To4(), Mask: net.CIDRMask(8, 32)})
	var settings webrtc.SettingEngine
	settings.SetNet(&loopbackNet{adapter: adapter})
	settings.SetIncludeLoopbackCandidate(true)
	settings.SetNetworkTypes([]webrtc.NetworkType{webrtc.NetworkTypeUDP4})
	return settings
}
