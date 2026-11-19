// The clients use actual Bedrock login, WebRTC channels and local player inputs.
package main

import (
	"context"
	"encoding/json"
	"flag"
	"fmt"
	"io"
	"log/slog"
	"math"
	"net"
	"net/http"
	"os"
	"sort"
	"strings"
	"sync"
	"sync/atomic"
	"time"

	"github.com/df-mc/dragonfly/server/world"
	"github.com/df-mc/go-nethernet"
	"github.com/df-mc/go-nethernet/endpoint"
	"github.com/go-gl/mathgl/mgl32"
	"github.com/google/uuid"
	"github.com/pion/ice/v4"
	"github.com/pion/webrtc/v4"
	"github.com/sandertv/gophertunnel/minecraft"
	"github.com/sandertv/gophertunnel/minecraft/protocol"
	"github.com/sandertv/gophertunnel/minecraft/protocol/packet"
)

var register sync.Mutex

type trace struct {
	sync.Mutex
	recent map[[2]uint32]time.Time
	ring   [256][2]uint32
	index  int
}

func positionKey(p mgl32.Vec3) [2]uint32 {
	return [2]uint32{math.Float32bits(p[0]), math.Float32bits(p[2])}
}
func (t *trace) sent(p mgl32.Vec3, now time.Time) {
	t.Lock()
	defer t.Unlock()
	key := positionKey(p)
	delete(t.recent, t.ring[t.index])
	t.ring[t.index] = key
	t.index = (t.index + 1) % len(t.ring)
	t.recent[key] = now
}
func (t *trace) stamp(p mgl32.Vec3) (time.Time, bool) {
	t.Lock()
	defer t.Unlock()
	stamp, ok := t.recent[positionKey(p)]
	return stamp, ok
}

type playerResult struct {
	SpawnMS              float64
	RuntimeID            uint64
	Survived             bool
	Echo, Motion, Blocks []float64
	Deaths               int
	RPG                  string
	Error                string
	Pigs, Sent           int
	MaxMotionGapMS       float64
}
type run struct {
	configured       sync.WaitGroup
	connect          chan struct{}
	goldID, goldHash uint32
	address          string
	count            int
	spawn, play      time.Duration
	start            time.Time
	ready            sync.WaitGroup
	playStart        time.Time
	goPlay           chan struct{}
	ids              []atomic.Uint64
	traces           []trace
	results          []playerResult
	markers          [3]atomic.Int64
}

func ms(d time.Duration) float64 { return float64(d) / float64(time.Millisecond) }
func (r *run) client(i int) {
	result := &r.results[i]
	arrived, configured := false, false
	defer func() {
		if !configured {
			r.configured.Done()
		}
		if !arrived {
			r.ready.Done()
		}
	}()
	ip := net.IPv4(127, 0, 0, byte(2+i))
	udp, err := net.ListenUDP("udp4", &net.UDPAddr{IP: ip})
	if err != nil {
		result.Error = err.Error()
		return
	}
	defer udp.Close()
	_ = udp.SetReadBuffer(1 << 20)
	_ = udp.SetWriteBuffer(1 << 20)
	mux := ice.NewUDPMuxDefault(ice.UDPMuxParams{UDPConn: udp})
	defer mux.Close()
	var settings webrtc.SettingEngine
	settings.SetIncludeLoopbackCandidate(true)
	settings.SetNetworkTypes([]webrtc.NetworkType{webrtc.NetworkTypeUDP4})
	settings.SetICEUDPMux(mux)
	settings.SetIPFilter(func(addr net.IP) bool { return addr.IsLoopback() })
	api := webrtc.NewAPI(webrtc.WithSettingEngine(settings))
	dialer := net.Dialer{LocalAddr: &net.TCPAddr{IP: ip}}
	httpTransport := &http.Transport{DialContext: dialer.DialContext}
	defer httpTransport.CloseIdleConnections()
	sig := (endpoint.ClientConfig{HTTPClient: &http.Client{Transport: httpTransport}}).New()
	name := fmt.Sprintf("burst%03d", i)
	register.Lock()
	minecraft.RegisterNetwork(name, func(log *slog.Logger) minecraft.Network {
		return minecraft.NetherNet{Log: log, Dialer: nethernet.Dialer{API: api}, Signaling: sig}
	})
	register.Unlock()
	r.configured.Done()
	configured = true
	<-r.connect
	d := minecraft.Dialer{ErrorLog: slog.New(slog.NewTextHandler(io.Discard, nil))}
	d.IdentityData.DisplayName = fmt.Sprintf("Load%03d", i)
	ctx, cancel := context.WithDeadline(context.Background(), r.start.Add(r.spawn))
	defer cancel()
	c, err := d.DialContext(ctx, name, "http://"+r.address)
	if err != nil {
		result.Error = err.Error()
		return
	}
	defer c.Close()
	c.SetReadDeadline(r.start.Add(r.spawn))
	if err = c.DoSpawn(); err != nil {
		result.Error = err.Error()
		return
	}
	result.SpawnMS = ms(time.Since(r.start))
	g := c.GameData()
	gold := r.goldID
	if g.UseBlockNetworkIDHashes {
		gold = r.goldHash
	}
	result.RuntimeID = g.EntityRuntimeID
	r.ids[i].Store(g.EntityRuntimeID)
	c.SetReadDeadline(time.Time{})
	var active atomic.Bool
	type sample struct {
		echo, motion, blocks []float64
		deaths, pigs         int
		rpg, err             string
		gap                  float64
	}
	incoming := make(chan sample, 1)
	go func() {
		var s sample
		var prev time.Time
		seen := [3]bool{}
		defer func() { incoming <- s }()
		for {
			pk, e := c.ReadPacket()
			now := time.Now()
			if e != nil {
				s.err = e.Error()
				return
			}
			switch v := pk.(type) {
			case *packet.NetworkStackLatency:
				if v.NeedsResponse {
					_ = c.WritePacket(&packet.NetworkStackLatency{Timestamp: v.Timestamp})
				} else if active.Load() && v.Timestamp > r.playStart.UnixNano() {
					s.echo = append(s.echo, ms(now.Sub(time.Unix(0, v.Timestamp))))
				}
			case *packet.MovePlayer:
				peer := (i + 1) % r.count
				if active.Load() && v.EntityRuntimeID == r.ids[peer].Load() && v.Mode == packet.MoveModeNormal {
					if sent, ok := r.traces[peer].stamp(v.Position); ok {
						s.motion = append(s.motion, ms(now.Sub(sent)))
						if !prev.IsZero() {
							s.gap = math.Max(s.gap, ms(now.Sub(prev)))
						}
						prev = now
					}
				}
			case *packet.AddActor:
				if v.EntityType == "minecraft:pig" || v.EntityType == "pig" {
					s.pigs++
				}
			case *packet.ActorEvent:
				if v.EntityRuntimeID == g.EntityRuntimeID && (v.EventType == packet.ActorEventDeath || v.EventType == packet.ActorEventInstantDeath) {
					s.deaths++
				}
			case *packet.SetHealth:
				if v.Health <= 0 {
					s.deaths++
				}
			case *packet.Text:
				if strings.Contains(v.Message, "RPG Lv.") {
					s.rpg = v.Message
				}
			case *packet.UpdateBlock:
				w := int(v.Position[0]) - 3
				if w >= 0 && w < 3 && !seen[w] && v.Position[1] == -59 && v.Position[2] == 4 && v.NewBlockRuntimeID == gold {
					if stamp := r.markers[w].Load(); stamp > 0 {
						seen[w] = true
						s.blocks = append(s.blocks, ms(now.Sub(time.Unix(0, stamp))))
					}
				}
			}
		}
	}()
	r.ready.Done()
	arrived = true
	keep := time.NewTicker(500 * time.Millisecond)
	defer keep.Stop()
wait:
	for {
		select {
		case <-r.goPlay:
			break wait
		case s := <-incoming:
			result.Error = s.err
			return
		case <-keep.C:
			_ = c.WritePacket(&packet.NetworkStackLatency{Timestamp: time.Now().UnixNano()})
			_ = c.Flush()
		}
	}
	active.Store(true)
	_ = c.WritePacket(&packet.CommandRequest{CommandLine: "/rpg", CommandOrigin: protocol.CommandOrigin{Origin: protocol.CommandOriginPlayer, UUID: uuid.New()}})
	flags := protocol.NewInputFlagsFromIDs(packet.InputFlagCount, []int32{packet.InputFlagVerticalCollision})
	steps := time.NewTicker(time.Second / 20)
	defer steps.Stop()
	deadline := time.NewTimer(time.Until(r.playStart.Add(r.play)))
	defer deadline.Stop()
	end := false
	var s sample
	for !end {
		select {
		case <-deadline.C:
			result.Survived = true
			c.Close()
			s = <-incoming
			end = true
		case s = <-incoming:
			result.Error = s.err
			end = true
		case <-steps.C:
			n := result.Sent + 1
			t := float64(n)
			p := g.PlayerPosition
			p[0] += float32(0.75 * math.Sin(t/10+float64(i)/4))
			p[2] += float32(0.60 * math.Sin(t/12+float64(i)/8))
			now := time.Now()
			r.traces[i].sent(p, now)
			err = c.WritePacket(&packet.PlayerAuthInput{Position: p, InputMode: packet.InputModeMouse, PlayMode: packet.PlayModeScreen, InteractionModel: packet.InteractionModelCrosshair, Tick: uint64(n), InputData: flags, CameraOrientation: mgl32.Vec3{0, 0, 1}})
			if err == nil && n%10 == 1 {
				err = c.WritePacket(&packet.NetworkStackLatency{Timestamp: now.UnixNano()})
			}
			if err == nil {
				err = c.Flush()
			}
			if err != nil {
				result.Error = err.Error()
				c.Close()
				s = <-incoming
				end = true
			} else {
				result.Sent++
			}
		}
	}
	result.Echo = s.echo
	result.Motion = s.motion
	result.Blocks = s.blocks
	result.Pigs = s.pigs
	result.Deaths = s.deaths
	result.RPG = s.rpg
	result.MaxMotionGapMS = s.gap
	if s.deaths > 0 {
		result.Survived = false
	}
}

func stats(data []float64) map[string]interface{} {
	sort.Float64s(data)
	result := map[string]interface{}{"samples": len(data)}
	for k, v := range map[string]float64{"p50_ms": .5, "p95_ms": .95, "p99_ms": .99, "max_ms": 1} {
		if len(data) > 0 {
			result[k] = data[int(math.Ceil(float64(len(data))*v))-1]
		}
	}
	return result
}

// Keep the test area loaded while the driver builds the arena on a copied world.
func dialSingle(address, name string) (*minecraft.Conn, error) {
	var settings webrtc.SettingEngine
	settings.SetIncludeLoopbackCandidate(true)
	settings.SetNetworkTypes([]webrtc.NetworkType{webrtc.NetworkTypeUDP4})
	api := webrtc.NewAPI(webrtc.WithSettingEngine(settings))
	minecraft.RegisterNetwork("arena", func(log *slog.Logger) minecraft.Network {
		return minecraft.NetherNet{Log: log, Dialer: nethernet.Dialer{API: api}}
	})
	d := minecraft.Dialer{}
	d.IdentityData.DisplayName = name
	d.IdentityData.Identity = uuid.NewSHA1(uuid.NameSpaceDNS, []byte(name)).String()
	ctx, cancel := context.WithTimeout(context.Background(), 45*time.Second)
	defer cancel()
	c, err := d.DialContext(ctx, "arena", "http://"+address)
	if err != nil {
		return nil, err
	}
	c.SetReadDeadline(time.Now().Add(45 * time.Second))
	if err = c.DoSpawn(); err != nil {
		c.Close()
		return nil, err
	}
	c.SetReadDeadline(time.Time{})
	return c, nil
}

func prepareWorld(address string) error {
	c, err := dialSingle(address, "ArenaBuilder")
	if err != nil {
		return err
	}
	defer c.Close()
	fmt.Printf("ARENA_BUILDER_READY %v\n", c.GameData().PlayerPosition)
	go func() {
		for {
			if _, err := c.ReadPacket(); err != nil {
				return
			}
		}
	}()
	_, err = io.Copy(io.Discard, os.Stdin)
	return err
}

func rpgSmoke(address string) error {
	player, err := dialSingle(address, "RpgProbe")
	if err != nil {
		return err
	}
	defer player.Close()
	query := func(c *minecraft.Conn, cmd, expect string) error {
		err := c.WritePacket(&packet.CommandRequest{CommandLine: cmd, CommandOrigin: protocol.CommandOrigin{Origin: protocol.CommandOriginPlayer, UUID: uuid.New()}})
		if err != nil {
			return err
		}
		if err = c.Flush(); err != nil {
			return err
		}
		c.SetReadDeadline(time.Now().Add(8 * time.Second))
		for {
			pk, err := c.ReadPacket()
			if err != nil {
				return fmt.Errorf("%s waiting for %q: %w", cmd, expect, err)
			}
			switch m := pk.(type) {
			case *packet.Text:
				if strings.Contains(m.Message, expect) {
					fmt.Println(m.Message)
					return nil
				}
			case *packet.NetworkStackLatency:
				if m.NeedsResponse {
					_ = c.WritePacket(&packet.NetworkStackLatency{Timestamp: m.Timestamp})
					_ = c.Flush()
				}
			}
		}
	}
	if err = query(player, "/rpg", "RPG Lv.1"); err != nil {
		return err
	}
	fmt.Println("RPG_AWARD")
	var acknowledged string
	if _, err = fmt.Fscanln(os.Stdin, &acknowledged); err != nil {
		return err
	}
	if err = query(player, "/rpg", "RPG Lv.2"); err != nil {
		return err
	}
	if err = query(player, "/rpg heal", "남은 회복 0회"); err != nil {
		return err
	}
	if err = query(player, "/rpg heal", "회복 횟수가 없습니다"); err != nil {
		return err
	}
	player.Close()
	time.Sleep(500 * time.Millisecond)
	next, err := dialSingle(address, "RpgProbe")
	if err != nil {
		return err
	}
	defer next.Close()
	if err = query(next, "/rpg", "회복 0회"); err != nil {
		return err
	}
	return query(next, "/rpg heal", "회복 횟수가 없습니다")
}

func main() {
	var r run
	var path string
	rpgCheck := flag.Bool("rpg-smoke", false, "check RPG XP, healing and reconnect persistence on a fresh test account")
	prepare := flag.Bool("prepare", false, "warm the copied arena until standard input closes")

	flag.StringVar(&r.address, "address", "127.0.0.1:19132", "local Sculith endpoint")
	flag.IntVar(&r.count, "players", 100, "simultaneous local players (2–250)")
	flag.DurationVar(&r.spawn, "spawn-timeout", 90*time.Second, "shared admission timeout")
	flag.DurationVar(&r.play, "play", 60*time.Second, "time in active play")
	flag.StringVar(&path, "output", "burst.json", "measurement report")

	flag.Parse()
	if r.count < 2 || r.count > 250 {
		panic("players must be 2–250")
	}
	ip, _, err := net.SplitHostPort(r.address)
	if err != nil || !net.ParseIP(ip).IsLoopback() {
		panic("this benchmark requires a loopback server")
	}
	if *rpgCheck {
		if err := rpgSmoke(r.address); err != nil {
			fmt.Fprintln(os.Stderr, err)
			os.Exit(1)
		}
		return
	}
	if *prepare {
		if err := prepareWorld(r.address); err != nil {
			fmt.Fprintln(os.Stderr, err)
			os.Exit(1)
		}
		return
	}
	registry := world.NewBlockRegistry()
	registry.Finalize()
	var found bool
	r.goldID, found = registry.StateToRuntimeID("minecraft:gold_block", nil)
	if !found {
		panic("missing gold block state")
	}
	r.goldHash, found = registry.RuntimeIDToHash(r.goldID)
	if !found {
		panic("missing gold block hash")
	}
	r.goPlay = make(chan struct{})
	r.results = make([]playerResult, r.count)
	r.traces = make([]trace, r.count)
	r.ids = make([]atomic.Uint64, r.count)
	for i := range r.traces {
		r.traces[i].recent = make(map[[2]uint32]time.Time, 256)
	}
	r.ready.Add(r.count)
	r.configured.Add(r.count)
	r.connect = make(chan struct{})
	var all sync.WaitGroup
	all.Add(r.count)
	for i := range r.count {
		go func() { defer all.Done(); r.client(i) }()
	}
	r.configured.Wait()
	r.start = time.Now()
	close(r.connect)
	r.ready.Wait()
	r.playStart = time.Now()
	close(r.goPlay)
	fmt.Printf("PLAY %d\n", r.playStart.UnixNano())

	go func() {

		for i := range r.markers {
			time.Sleep(time.Until(r.playStart.Add(r.play * time.Duration(i+1) / 4)))
			ts := time.Now().UnixNano()
			r.markers[i].Store(ts)
			fmt.Printf("MARK %d %d\n", i, ts)
		}
	}()
	all.Wait()
	var spawns, motion, echo, blocks, gaps []float64
	var admitted, alive, peers, responded, allBlocks, sawMobs, sent int
	errors := map[string]string{}
	for i, p := range r.results {
		if p.SpawnMS > 0 {
			admitted++
			spawns = append(spawns, p.SpawnMS)
		}
		if p.Survived {
			alive++
		}
		if len(p.Motion) > 0 {
			peers++
			gaps = append(gaps, p.MaxMotionGapMS)
		}
		if len(p.Blocks) == 3 {
			allBlocks++
		}
		if p.Pigs > 0 {
			sawMobs++
		}
		if p.RPG != "" {
			responded++
		}
		if p.Error != "" {
			errors[fmt.Sprint(i)] = p.Error
		}
		sent += p.Sent
		motion = append(motion, p.Motion...)
		echo = append(echo, p.Echo...)
		blocks = append(blocks, p.Blocks...)
	}
	report := map[string]interface{}{"players": r.count, "spawned": admitted, "survived": alive, "duration_s": r.play.Seconds(), "inputs_sent": sent, "viewers_with_matched_peer": peers, "rpg_replies": responded, "viewers_receiving_three_markers": allBlocks, "viewers_receiving_pigs": sawMobs, "spawn": stats(spawns), "peer_input_to_visible_motion": stats(motion), "echo_round_trip": stats(echo), "world_block_to_visible_update": stats(blocks), "per_viewer_max_motion_gap": stats(gaps), "client_errors": errors}
	bytes, _ := json.MarshalIndent(report, "", "  ")
	if err = os.WriteFile(path, bytes, 0644); err != nil {
		panic(err)
	}
	fmt.Println(string(bytes))
	if alive != r.count || peers != r.count {
		os.Exit(1)
	}
}
