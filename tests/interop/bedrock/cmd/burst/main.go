// The clients use actual Bedrock login, WebRTC channels and local player inputs.
package main

import (
	"context"
	"encoding/binary"
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
	LODAdds, LODPeak, LODInvalid int
	LODScales                    [3]bool
	OrderedPeer                  bool
	RosterAdds                   int
	HealMS                       float64
	SpawnMS                      float64
	RuntimeID                    uint64
	Survived                     bool
	Echo, Motion, Blocks         []float64
	Deaths                       int
	RPG                          string
	Error                        string
	Pigs, Sent                   int
	Chunks, UniqueChunks         int
	ChunkRadius                  int
	PackBytes                    int
	PackDigest                   string
	WorstGap                     motionGap
}

type motionGap struct {
	DurationMS     float64  `json:"duration_ms"`
	EndAfterPlayMS float64  `json:"end_after_play_start_ms"`
	Boundary       string   `json:"boundary"`
	InputGapMS     *float64 `json:"matched_input_spacing_ms,omitempty"`
}

func (g *motionGap) record(start, prev, now, prevInput, input time.Time, boundary string) {
	duration := ms(now.Sub(prev))
	if duration <= g.DurationMS {
		return
	}
	*g = motionGap{DurationMS: duration, EndAfterPlayMS: ms(now.Sub(start)), Boundary: boundary}
	if !prevInput.IsZero() && !input.IsZero() {
		inputGap := ms(input.Sub(prevInput))
		g.InputGapMS = &inputGap
	}
}

type run struct {
	markerX, markerY, markerZ int
	encounter                 bool
	configured                sync.WaitGroup
	connect                   chan struct{}
	goldID, goldHash          uint32
	address                   string
	count                     int
	spawn, play               time.Duration
	start                     time.Time
	ready                     sync.WaitGroup
	playStart                 time.Time
	goPlay                    chan struct{}
	ids                       []atomic.Uint64
	traces                    []trace
	results                   []playerResult
	markers                   [3]atomic.Int64
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
	settings := loopbackSettings(ip)
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
	result.ChunkRadius = c.ChunkRadius()
	if packs := c.ResourcePacks(); len(packs) == 1 {
		result.PackBytes = packs[0].Len()
		result.PackDigest = fmt.Sprintf("%x", packs[0].Checksum())
	}
	gold := r.goldID
	if g.UseBlockNetworkIDHashes {
		gold = r.goldHash
	}
	result.RuntimeID = g.EntityRuntimeID
	r.ids[i].Store(g.EntityRuntimeID)
	c.SetReadDeadline(time.Time{})
	var active atomic.Bool
	var healSent atomic.Int64
	type sample struct {
		echo, motion, blocks               []float64
		deaths, pigs, chunks, uniqueChunks int
		lodAdds, lodPeak, lodInvalid       int
		lodScales                          [3]bool
		rpg, err                           string
		healMS                             float64
		gap                                motionGap
	}
	incoming := make(chan sample, 1)
	go func() {
		var s sample
		known := make(map[uuid.UUID]bool)
		actors := make(map[uint64]bool)
		rosterAdds := 0
		terrain := make(map[protocol.ChunkPos]struct{})
		lod := make(map[uint64]struct{})
		var prev, prevInput time.Time
		seen := [3]bool{}
		defer func() {
			s.uniqueChunks = len(terrain)
			if active.Load() {
				if prev.IsZero() {
					prev = r.playStart
				}
				s.gap.record(r.playStart, prev, r.playStart.Add(r.play), prevInput, time.Time{}, "play_end")
			}
			result.OrderedPeer = actors[r.ids[(i+1)%r.count].Load()]
			result.RosterAdds = rosterAdds
			incoming <- s
		}()
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
						boundary := "between_updates"
						if prev.IsZero() {
							prev = r.playStart
							boundary = "play_start"
						}
						s.gap.record(r.playStart, prev, now, prevInput, sent, boundary)
						prev, prevInput = now, sent
					}
				}
			case *packet.PlayerList:
				for _, entry := range v.Entries {
					if entry.ActionType == protocol.PlayerListActionAdd {
						known[entry.UUID] = true
						rosterAdds++
					} else {
						delete(known, entry.UUID)
					}
				}
			case *packet.AddPlayer:
				actors[v.EntityRuntimeID] = known[v.UUID]
			case *packet.RemoveActor:
				id := uint64(v.EntityUniqueID)
				if id>>58 == 1 {
					if _, ok := lod[id]; !ok {
						s.lodInvalid++
					}
					delete(lod, id)
				}
			case *packet.MoveActorAbsolute:
				if v.EntityRuntimeID>>58 == 1 {
					if _, ok := lod[v.EntityRuntimeID]; !ok {
						s.lodInvalid++
					}
				}
			case *packet.AddActor:
				if strings.HasPrefix(v.EntityType, "sculith:lod_") {
					id := v.EntityRuntimeID
					width, ok := v.EntityMetadata[protocol.EntityDataKeyScale].(float32)
					_, dupe := lod[id]
					if dupe || id != uint64(v.EntityUniqueID) || id>>58 != 1 || !ok || (width != 8 && width != 16 && width != 32) {
						s.lodInvalid++
					}
					switch width {
					case 8:
						s.lodScales[0] = true
					case 16:
						s.lodScales[1] = true
					case 32:
						s.lodScales[2] = true
					}
					lod[id] = struct{}{}
					s.lodAdds++
					s.lodPeak = max(s.lodPeak, len(lod))
				}
				if v.EntityType == "minecraft:pig" || v.EntityType == "pig" {
					s.pigs++
				}
			case *packet.LevelChunk:
				s.chunks++
				terrain[v.Position] = struct{}{}
			case *packet.ActorEvent:
				if v.EntityRuntimeID == g.EntityRuntimeID && (v.EventType == packet.ActorEventDeath || v.EventType == packet.ActorEventInstantDeath) {
					s.deaths++
				}
			case *packet.SetHealth:
				if v.Health <= 0 {
					s.deaths++
				}
			case *packet.Text:
				if stamp := healSent.Load(); stamp > 0 && strings.Contains(v.Message, "남은 회복 0회") && s.healMS == 0 {
					s.healMS = ms(now.Sub(time.Unix(0, stamp)))
				}
				if strings.Contains(v.Message, "RPG Lv.") {
					s.rpg = v.Message
				}
			case *packet.UpdateBlock:
				w := int(v.Position[0]) - r.markerX
				if w >= 0 && w < 3 && !seen[w] && int(v.Position[1]) == r.markerY && int(v.Position[2]) == r.markerZ && v.NewBlockRuntimeID == gold {
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
			if r.encounter && time.Since(r.playStart) >= r.play*3/4 && healSent.Load() == 0 {
				healSent.Store(time.Now().UnixNano())
				_ = c.WritePacket(&packet.CommandRequest{CommandLine: "/rpg heal", CommandOrigin: protocol.CommandOrigin{Origin: protocol.CommandOriginPlayer, UUID: uuid.New()}})
			}
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
	result.LODScales = s.lodScales
	result.LODAdds, result.LODPeak, result.LODInvalid = s.lodAdds, s.lodPeak, s.lodInvalid
	result.Chunks = s.chunks
	result.UniqueChunks = s.uniqueChunks
	result.Deaths = s.deaths
	result.RPG = s.rpg
	result.HealMS = s.healMS
	result.WorstGap = s.gap
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

func dialSingle(address, name string, threshold *atomic.Int32) (*minecraft.Conn, error) {
	settings := loopbackSettings(net.IPv4(127, 0, 0, 1))
	api := webrtc.NewAPI(webrtc.WithSettingEngine(settings))
	minecraft.RegisterNetwork("arena", func(log *slog.Logger) minecraft.Network {
		return minecraft.NetherNet{Log: log, Dialer: nethernet.Dialer{API: api}}
	})
	d := minecraft.Dialer{}
	if threshold != nil {
		d.PacketFunc = func(h packet.Header, payload []byte, _, _ net.Addr) {
			if h.PacketID == packet.IDNetworkSettings && len(payload) >= 2 {
				threshold.Store(int32(binary.LittleEndian.Uint16(payload)))
			}
		}
	}
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
	var threshold atomic.Int32
	threshold.Store(-1)
	c, err := dialSingle(address, "ArenaBuilder", &threshold)
	if err != nil {
		return err
	}
	defer c.Close()
	fmt.Printf("ARENA_BUILDER_READY %v threshold=%d\n", c.GameData().PlayerPosition, threshold.Load())
	done := make(chan struct{})
	defer close(done)
	go func() {
		heartbeat := time.NewTicker(time.Second)
		defer heartbeat.Stop()
		for {
			select {
			case <-done:
				return
			case now := <-heartbeat.C:
				if c.WritePacket(&packet.NetworkStackLatency{Timestamp: now.UnixNano(), NeedsResponse: true}) != nil || c.Flush() != nil {
					return
				}
			}
		}
	}()
	go func() {
		for {
			p, err := c.ReadPacket()
			if err != nil {
				return
			}
			if latency, ok := p.(*packet.NetworkStackLatency); ok && latency.NeedsResponse {
				_ = c.WritePacket(&packet.NetworkStackLatency{Timestamp: latency.Timestamp})
				_ = c.Flush()
			}
		}
	}()
	_, err = io.Copy(io.Discard, os.Stdin)
	return err
}

func rpgSmoke(address string) error {
	player, err := dialSingle(address, "RpgProbe", nil)
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
	next, err := dialSingle(address, "RpgProbe", nil)
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
	flag.BoolVar(&r.encounter, "encounter", false, "activate the RPG heal after a server-driven XP and damage wave")
	rpgCheck := flag.Bool("rpg-smoke", false, "check RPG XP, healing and reconnect persistence on a fresh test account")
	terrainCheck := flag.Bool("terrain-smoke", false, "check the chunk publisher and terrain after a round-trip teleport")
	prepare := flag.Bool("prepare", false, "warm the copied arena until standard input closes")

	flag.StringVar(&r.address, "address", "127.0.0.1:19132", "local Sculith endpoint")
	flag.IntVar(&r.count, "players", 100, "simultaneous local players (2–250)")
	flag.DurationVar(&r.spawn, "spawn-timeout", 90*time.Second, "shared admission timeout")
	flag.DurationVar(&r.play, "play", 60*time.Second, "time in active play")
	flag.StringVar(&path, "output", "burst.json", "measurement report")

	flag.IntVar(&r.markerX, "marker-x", 3, "first edited block X")
	flag.IntVar(&r.markerY, "marker-y", -59, "edited block Y")
	flag.IntVar(&r.markerZ, "marker-z", 4, "edited block Z")
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
	if *terrainCheck {
		if err := checkTerrainTravel(r.address); err != nil {
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
	fmt.Println("JOIN")
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
	fmt.Println("FINISHED")
	var spawns, motion, echo, blocks, gaps, viewerP95, inputCoverage, heals []float64
	var admitted, alive, peers, responded, allBlocks, sawMobs, sent, ordered, adds int
	var chunks, uniqueChunks, minChunks, maxChunks, fullView, minInputs int
	errors := map[string]string{}
	packCopies := map[string]int{}
	var packBytes int64
	var lodViewers, lodFull, lodPeak, lodInvalid, lodAdds int
	var scaleViewers [3]int
	var worst motionGap
	var worstViewer int
	for i, p := range r.results {
		if p.PackDigest != "" {
			packCopies[p.PackDigest]++
			packBytes += int64(p.PackBytes)
		}
		chunks += p.Chunks
		uniqueChunks += p.UniqueChunks
		if i == 0 || p.Chunks < minChunks {
			minChunks = p.Chunks
		}
		if i == 0 || p.Sent < minInputs {
			minInputs = p.Sent
		}
		maxChunks = max(maxChunks, p.Chunks)
		if p.ChunkRadius == 16 {
			fullView++
		}
		if p.OrderedPeer {
			ordered++
		}
		adds += p.RosterAdds
		if p.HealMS > 0 {
			heals = append(heals, p.HealMS)
		}
		if p.SpawnMS > 0 {
			admitted++
			spawns = append(spawns, p.SpawnMS)
		}
		if p.Survived {
			alive++
		}
		if len(p.Motion) > 0 {
			peers++
			sorted := append([]float64(nil), p.Motion...)
			sort.Float64s(sorted)
			viewerP95 = append(viewerP95, sorted[int(math.Ceil(.95*float64(len(sorted))))-1])
			peerSent := r.results[(i+1)%r.count].Sent
			if peerSent > 0 {
				inputCoverage = append(inputCoverage, 100*float64(len(p.Motion))/float64(peerSent))
			}
			gaps = append(gaps, p.WorstGap.DurationMS)
			if p.WorstGap.DurationMS > worst.DurationMS {
				worst, worstViewer = p.WorstGap, i
			}
		}
		if len(p.Blocks) == 3 {
			allBlocks++
		}
		if p.LODAdds > 0 {
			lodViewers++
		}
		for j, seen := range p.LODScales {
			if seen {
				scaleViewers[j]++
			}
		}
		if p.LODScales == [3]bool{true, true, true} {
			lodFull++
		}
		lodPeak = max(lodPeak, p.LODPeak)
		lodInvalid += p.LODInvalid
		lodAdds += p.LODAdds
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
	report["viewers_receiving_lod_tiles"] = lodViewers
	report["viewers_receiving_all_lod_scales"] = lodFull
	report["lod_viewers_by_tile_width"] = map[string]int{"8": scaleViewers[0], "16": scaleViewers[1], "32": scaleViewers[2]}
	report["lod_tile_spawns"] = lodAdds
	report["lod_peak_per_viewer"] = lodPeak
	report["lod_invalid_sequences"] = lodInvalid
	report["per_viewer_motion_p95"] = stats(viewerP95)
	report["rpg_heal_feedback"] = stats(heals)
	report["known_peer_before_actor_spawn"] = ordered
	report["roster_add_entries_seen"] = adds
	if worst.DurationMS > 0 {
		report["worst_motion_gap"] = struct {
			motionGap
			Viewer int `json:"viewer"`
			Peer   int `json:"peer"`
		}{worst, worstViewer, (worstViewer + 1) % r.count}
	}
	report["resource_pack_copies_by_sha256"] = packCopies
	report["resource_pack_bytes_received"] = packBytes
	report["clients_granted_radius_16"] = fullView
	report["minimum_input_rate_percent"] = 100 * float64(minInputs) / (r.play.Seconds() * 20)
	report["level_chunk_messages_read"] = map[string]int{"total": chunks, "distinct_positions_summed_across_viewers": uniqueChunks, "repeated_positions": chunks - uniqueChunks, "minimum_per_viewer": minChunks, "maximum_per_viewer": maxChunks}
	if len(inputCoverage) > 0 {
		sort.Float64s(inputCoverage)
		report["minimum_matched_peer_inputs_percent"] = inputCoverage[0]
	}
	bytes, _ := json.MarshalIndent(report, "", "  ")
	if err = os.WriteFile(path, bytes, 0644); err != nil {
		panic(err)
	}
	fmt.Println(string(bytes))
	if alive != r.count || peers != r.count {
		os.Exit(1)
	}
}
