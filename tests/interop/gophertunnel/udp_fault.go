package main

import (
	"log/slog"
	"os"

	"github.com/df-mc/go-nethernet"
	"github.com/pion/webrtc/v4"
	"github.com/sandertv/gophertunnel/minecraft"
)

func probeNetwork() string {
	if os.Getenv("SCULITH_TEST_LOOPBACK_CANDIDATE") != "1" {
		return "nethernet"
	}
	// Supply a healthy alternative even on CI hosts with only one physical NIC.
	// All other probes retain the independent client's ordinary configuration.
	var settings webrtc.SettingEngine
	settings.SetIncludeLoopbackCandidate(true)
	settings.SetNetworkTypes([]webrtc.NetworkType{webrtc.NetworkTypeUDP4})
	api := webrtc.NewAPI(webrtc.WithSettingEngine(settings))
	minecraft.RegisterNetwork("nethernet-fault", func(log *slog.Logger) minecraft.Network {
		return minecraft.NetherNet{Log: log, Dialer: nethernet.Dialer{API: api}}
	})
	return "nethernet-fault"
}
