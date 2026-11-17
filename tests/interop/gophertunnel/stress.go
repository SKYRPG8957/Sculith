package main

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"log/slog"
	"math"
	"net"
	"net/http"
	"net/url"
	"os"
	"sync"
	"time"

	"github.com/df-mc/dragonfly/server/world"
	"github.com/df-mc/go-nethernet/endpoint"
	"github.com/go-gl/mathgl/mgl32"
	"github.com/sandertv/gophertunnel/minecraft"
	"github.com/sandertv/gophertunnel/minecraft/protocol"
	"github.com/sandertv/gophertunnel/minecraft/protocol/packet"
)

// Local load generator, not an official GDK client or a player-capacity claim.
// Each connection receives the actual NetherNet/SCTP world stream, then sends
// 20 movement inputs/s, one 67-packet burst and latency fences throughout play.
type stressResult struct {
	Client        int       `json:"client"`
	Error         string    `json:"error,omitempty"`
	DialMS        float64   `json:"dial_ms"`
	SpawnMS       float64   `json:"spawn_ms"`
	FirstChunkMS  float64   `json:"first_chunk_ms"`
	LastChunkMS   float64   `json:"last_chunk_ms"`
	CenterChunkMS float64   `json:"center_chunk_ms"`
	Chunks        int       `json:"chunks"`
	ChunkBytes    int       `json:"chunk_payload_bytes"`
	Moves         int       `json:"movement_inputs_sent"`
	PeerMoves     int       `json:"peer_movements_received"`
	Corrections   int       `json:"movement_corrections"`
	BlockBreakAck bool      `json:"block_break_ack"`
	BurstAck      bool      `json:"burst_tail_ack"`
	RTTMS         []float64 `json:"control_rtt_ms"`
	ElapsedMS     float64   `json:"elapsed_ms"`
}

func nativeStress(address string, clients, seconds int) error {
	u, err := url.Parse(address)
	if err != nil || u.Scheme != "http" || net.ParseIP(u.Hostname()) == nil || !net.ParseIP(u.Hostname()).IsLoopback() || clients < 1 || clients > 64 || seconds < 1 || seconds > 300 {
		return fmt.Errorf("stress requires a loopback http address, 1..64 clients and 1..300 seconds")
	}
	// Preserve the server's four-sessions-per-IP admission limit. Linux loopback
	// permits separate local source addresses without changing server policy.
	networks := make([]string, clients)
	for index := range networks {
		local := &net.TCPAddr{IP: net.IPv4(127, 0, 0, byte(index+1))}
		transport := &http.Transport{DialContext: (&net.Dialer{LocalAddr: local, Timeout: 10 * time.Second}).DialContext}
		defer transport.CloseIdleConnections()
		signaling := (endpoint.ClientConfig{HTTPClient: &http.Client{Transport: transport, Timeout: 30 * time.Second}}).New()
		networks[index] = fmt.Sprintf("nethernet-load-%02d", index)
		minecraft.RegisterNetwork(networks[index], func(log *slog.Logger) minecraft.Network {
			return minecraft.NetherNet{Log: log, Signaling: signaling}
		})
	}
	registry := world.NewBlockRegistry()
	registry.Finalize()
	airID, ok := registry.StateToRuntimeID("minecraft:air", nil)
	if !ok {
		return fmt.Errorf("independent registry has no air")
	}
	airHash, ok := registry.RuntimeIDToHash(airID)
	if !ok {
		return fmt.Errorf("independent registry has no air hash")
	}
	results := make([]stressResult, clients)
	var ready, workers sync.WaitGroup
	ready.Add(clients)
	workers.Add(clients)
	start := make(chan struct{})
	joins := make(chan struct{}, 8)
	for index := range results {
		go func(index int) {
			defer workers.Done()
			joins <- struct{}{}
			results[index] = stressClient(address, networks[index], index, airHash, time.Duration(seconds)*time.Second, &ready, start, func() { <-joins })
		}(index)
	}
	ready.Wait()
	close(start)
	workers.Wait()
	failures := 0
	for _, result := range results {
		if result.Error != "" {
			failures++
		}
	}
	if err := json.NewEncoder(os.Stdout).Encode(struct {
		Schema          int            `json:"schema_version"`
		Scope           string         `json:"scope"`
		Clients         int            `json:"clients"`
		Seconds         int            `json:"active_seconds"`
		Failures        int            `json:"failures"`
		Sources         string         `json:"source_addresses"`
		ConcurrentJoins int            `json:"max_concurrent_joins"`
		Results         []stressResult `json:"results"`
	}{1, "loopback_nethernet_20hz", clients, seconds, failures, "distinct_loopback_ipv4", 8, results}); err != nil {
		return err
	}
	if failures != 0 {
		return fmt.Errorf("%d/%d native load clients failed", failures, clients)
	}
	return nil
}

func stressClient(address, network string, index int, airHash uint32, duration time.Duration, ready *sync.WaitGroup, start <-chan struct{}, releaseJoin func()) (result stressResult) {
	result.Client = index
	began := time.Now()
	readyReported := false
	defer func() {
		result.ElapsedMS = float64(time.Since(began)) / float64(time.Millisecond)
		if !readyReported {
			ready.Done()
			releaseJoin()
		}
	}()
	fail := func(err error) stressResult { result.Error = err.Error(); return result }
	var mu sync.Mutex
	chunks, chunkBytes := 0, 0
	var first, last, center time.Duration
	dialer := minecraft.Dialer{FlushRate: -1}
	dialer.IdentityData.DisplayName = fmt.Sprintf("Load%02d", index)
	dialer.PacketFunc = func(header packet.Header, payload []byte, _, _ net.Addr) {
		if header.PacketID == packet.IDLevelChunk {
			mu.Lock()
			defer mu.Unlock()
			chunks++
			chunkBytes += len(payload)
			last = time.Since(began)
			if chunks == 1 {
				first = last
			}
			var chunk packet.LevelChunk
			chunk.Marshal(protocol.NewReader(bytes.NewBuffer(payload), 0, true))
			if chunk.Position == (protocol.ChunkPos{0, 0}) {
				center = last
			}
		}
	}
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()
	conn, err := dialer.DialContext(ctx, network, address)
	if err != nil {
		return fail(fmt.Errorf("dial: %w", err))
	}
	defer conn.Close()
	result.DialMS = float64(time.Since(began)) / float64(time.Millisecond)
	_ = conn.SetReadDeadline(time.Now().Add(20 * time.Second))
	if err = conn.DoSpawn(); err != nil {
		return fail(fmt.Errorf("spawn: %w", err))
	}
	if err = conn.Flush(); err != nil {
		return fail(err)
	}
	result.SpawnMS = float64(time.Since(began)) / float64(time.Millisecond)
	mu.Lock()
	result.Chunks, result.ChunkBytes = chunks, chunkBytes
	result.FirstChunkMS, result.LastChunkMS = float64(first)/float64(time.Millisecond), float64(last)/float64(time.Millisecond)
	result.CenterChunkMS = float64(center) / float64(time.Millisecond)
	mu.Unlock()
	if result.Chunks != 25 {
		return fail(fmt.Errorf("got %d initial chunks, want 25", result.Chunks))
	}
	readyReported = true
	releaseJoin()
	ready.Done()
	<-start
	_ = conn.SetReadDeadline(time.Now().Add(duration + 10*time.Second))
	// A single explicit flush must preserve the complete burst, including its
	// last packet. Automatic flushing is disabled so batching is deterministic.
	for i := 0; i < 66; i++ {
		if err = conn.WritePacket(&packet.ClientCacheStatus{}); err != nil {
			return fail(err)
		}
	}
	burstSent := time.Now()
	stamp := burstSent.UnixNano()
	if err = conn.WritePacket(&packet.NetworkStackLatency{Timestamp: stamp, NeedsResponse: true}); err != nil {
		return fail(err)
	}
	if err = conn.Flush(); err != nil {
		return fail(err)
	}
	for _, action := range []uint32{protocol.UseItemActionClickBlock, protocol.UseItemActionClickAir} {
		face := int32(1)
		if action == protocol.UseItemActionClickAir {
			face = -1
		}
		if err = conn.WritePacket(&packet.InventoryTransaction{TransactionData: &protocol.UseItemTransactionData{
			ActionType: action, BlockPosition: protocol.BlockPos{0, 3, 1}, BlockFace: face,
			Position: mgl32.Vec3{.5, 5.62, .5}, ClickedPosition: mgl32.Vec3{.5, 1, .5},
		}}); err != nil {
			return fail(err)
		}
	}
	if err = conn.Flush(); err != nil {
		return fail(err)
	}
	received := make(chan packet.Packet, 512)
	readError := make(chan error, 1)
	done := make(chan struct{})
	defer close(done)
	go func() {
		for {
			pk, err := conn.ReadPacket()
			if err != nil {
				readError <- err
				return
			}
			select {
			case received <- pk:
			case <-done:
				return
			}
		}
	}()
	// Distinct exposed ground blocks inside the existing six-block eye reach.
	// The old 8x8 grid's far corner exceeded reach; that was a probe failure.
	var targets []protocol.BlockPos
	for z := int32(-5); z <= 5; z++ {
		for x := int32(-5); x <= 5; x++ {
			if x*x+z*z <= 25 {
				targets = append(targets, protocol.BlockPos{x, 3, z})
			}
		}
	}
	position := targets[index]
	if err = conn.WritePacket(&packet.PlayerAction{EntityRuntimeID: conn.GameData().EntityRuntimeID, ActionType: protocol.PlayerActionPredictDestroyBlock, BlockPosition: position, ResultPosition: position, BlockFace: 1}); err != nil {
		return fail(err)
	}
	if err = conn.Flush(); err != nil {
		return fail(err)
	}
	latency := map[int64]time.Time{stamp: burstSent}
	move := time.NewTicker(time.Second / 20)
	defer move.Stop()
	ping := time.NewTicker(time.Second)
	defer ping.Stop()
	end := time.NewTimer(duration)
	defer end.Stop()
	active := true
	var tick uint64
	finish := time.NewTimer(duration + 5*time.Second)
	defer finish.Stop()
	for {
		if !active && len(latency) == 0 && result.BlockBreakAck && result.BurstAck {
			return result
		}
		select {
		case err = <-readError:
			return fail(err)
		case <-finish.C:
			return fail(fmt.Errorf("missing replies: burst=%v break=%v outstanding_fences=%d", result.BurstAck, result.BlockBreakAck, len(latency)))
		case <-end.C:
			active = false
			move.Stop()
			ping.Stop()
			timestamp := time.Now().UnixNano()
			latency[timestamp] = time.Now()
			if err = conn.WritePacket(&packet.NetworkStackLatency{Timestamp: timestamp, NeedsResponse: true}); err != nil {
				return fail(err)
			}
			if err = conn.Flush(); err != nil {
				return fail(err)
			}
		case <-move.C:
			if !active {
				continue
			}
			tick++
			x := .5 + .25*math.Sin(float64(tick)*.1)
			pk := &packet.PlayerAuthInput{Position: mgl32.Vec3{float32(x), 5.62, .5}, InputMode: packet.InputModeMouse, PlayMode: packet.PlayModeScreen, InteractionModel: packet.InteractionModelCrosshair, Tick: tick, CameraOrientation: mgl32.Vec3{0, 0, 1}}
			if err = conn.WritePacket(pk); err != nil {
				return fail(err)
			}
			if err = conn.Flush(); err != nil {
				return fail(err)
			}
			result.Moves++
		case now := <-ping.C:
			if !active {
				continue
			}
			timestamp := now.UnixNano()
			latency[timestamp] = time.Now()
			if err = conn.WritePacket(&packet.NetworkStackLatency{Timestamp: timestamp, NeedsResponse: true}); err != nil {
				return fail(err)
			}
			if err = conn.Flush(); err != nil {
				return fail(err)
			}
		case pk := <-received:
			switch p := pk.(type) {
			case *packet.NetworkStackLatency:
				if sent, exists := latency[p.Timestamp]; exists && !p.NeedsResponse {
					result.RTTMS = append(result.RTTMS, float64(time.Since(sent))/float64(time.Millisecond))
					delete(latency, p.Timestamp)
					if p.Timestamp == stamp {
						result.BurstAck = true
					}
				}
			case *packet.UpdateBlock:
				if p.Position == position {
					if p.NewBlockRuntimeID != airHash {
						return fail(fmt.Errorf("block break was corrected instead of committed"))
					}
					result.BlockBreakAck = true
				}
			case *packet.MovePlayer:
				if p.EntityRuntimeID == conn.GameData().EntityRuntimeID {
					result.Corrections++
				} else {
					result.PeerMoves++
				}
			case *packet.Disconnect:
				return fail(fmt.Errorf("disconnected: %s", p.Message))
			}
		}
	}
}
