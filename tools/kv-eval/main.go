package main

import (
	"bytes"
	"context"
	"encoding/json"
	"flag"
	"fmt"
	pinglog "github.com/pingcap/log"
	"os"
	"regexp"
	"strings"
	"time"
)

type RegionInfo struct {
	ID            uint64
	LeaderStore   uint64
	LeaderAddress string
	Peers         int
}

func topology(s *tikvStore, key []byte) RegionInfo {
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	region, err := s.client.GetPDClient().GetRegion(ctx, key)
	checked(err)
	ensure(region != nil && region.Meta != nil && region.Leader != nil, "region or leader missing")
	store, err := s.client.GetPDClient().GetStore(ctx, region.Leader.StoreId)
	checked(err)
	return RegionInfo{ID: region.Meta.Id, LeaderStore: region.Leader.StoreId, LeaderAddress: store.Address, Peers: len(region.Meta.Peers)}
}
func prepareRegions(a Adapter) map[string]RegionInfo {
	s := a.Store.(*tikvStore)
	var split [][]byte
	for _, prefix := range []string{"count", "logs", "meta", "once", "refs"} {
		split = append(split, []byte(a.Root+"/"+prefix+"/"))
	}
	for shard := range 8 {
		repo := []byte(fmt.Sprintf("bench-cas-8/partition-%d", shard))
		for _, prefix := range []string{"count", "logs", "once", "refs"} {
			split = append(split, []byte(a.Root+"/"+prefix+"/"+digest(repo)+"/"))
		}
	}
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	regionIDs, err := s.client.SplitRegions(ctx, split, true, nil)
	cancel()
	checked(err)
	keys := map[string][]byte{"ref": a.refKey([]byte("fault"), "native", []byte("main")), "log": a.logPrefix([]byte("fault"), "native"), "receipt": a.receiptKey([]byte("fault"), []byte("base")), "counter": a.counterKey([]byte("fault"), "native")}
	for shard := range 8 {
		repo := []byte(fmt.Sprintf("bench-cas-8/partition-%d", shard))
		keys[fmt.Sprintf("shardCounter%d", shard)] = a.counterKey(repo, "native")
	}
	deadline := time.Now().Add(90 * time.Second)
	for {
		out := map[string]RegionInfo{}
		ids := map[uint64]bool{}
		ready := true
		counterRegions := map[uint64]bool{}
		counterLeaders := map[uint64]bool{}
		for name, key := range keys {
			info := topology(s, key)
			out[name] = info
			if strings.HasPrefix(name, "shardCounter") {
				counterRegions[info.ID] = true
				counterLeaders[info.LeaderStore] = true
			} else {
				ids[info.ID] = true
			}
			if info.Peers != 3 {
				ready = false
			}
		}
		for _, id := range regionIDs {
			ctx, cancel := context.WithTimeout(context.Background(), 3*time.Second)
			operator, err := s.client.GetPDClient().GetOperator(ctx, id)
			cancel()
			checked(err)
			if operator != nil && operator.GetStatus().String() == "RUNNING" {
				ready = false
			}
		}
		if ready && len(ids) == 4 && len(counterRegions) == 8 && len(counterLeaders) >= 2 {
			// Warm region routes and gRPC connections after scatter, without
			// modifying any measured ref/counter or counting startup work.
			for range 2 {
				for _, key := range keys {
					ctx, cancel := opContext()
					tx, err := a.Store.Begin(ctx)
					checked(err)
					_, err = tx.Get(ctx, key)
					checked(err)
					rollback(tx)
					cancel()
				}
			}
			return out
		}
		ensure(time.Now().Before(deadline), "split regions never reached three replicas")
		time.Sleep(250 * time.Millisecond)
	}
}
func verify(a Adapter, n int) {
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()
	key := append(bytes.Repeat([]byte{0xff}, 8192), 0)
	value, err := a.Get(ctx, key)
	checked(err)
	ensure(value != nil && len(value) == 0, "long key/empty value missing after restart")
	for _, kind := range []string{"native", "git"} {
		ensure(len(logs(a, "mixed-race", kind)) == 1, "mixed race lost its log")
		ensure(len(logs(a, "receipt-race", kind)) == 1, "receipt replay count changed")
		ensure(len(logs(a, "lost-reply", kind)) == 1, "lost reply duplicated or vanished")
		ensure(len(logs(a, "rollback", kind)) == 1, "rollback retry log count changed")
		ensure(read(a, "aba-"+kind, kind, "ref").Version == 3, "ABA tombstone changed after restart")
	}
	for i := range n {
		key := []byte(fmt.Sprintf("bench/object/%d", i))
		got, err := a.Get(ctx, key)
		checked(err)
		ensure(len(got) == 4096, "benchmark immutable disappeared")
		for j, b := range got {
			ensure(b == byte((j*131+17)%251), "benchmark immutable changed")
		}
	}
	for _, shards := range []int{1, 8} {
		total := 0
		for i := range n {
			repo := fmt.Sprintf("bench-cas-%d/partition-%d", shards, i%shards)
			state := read(a, repo, "native", fmt.Sprintf("ref-%d", i))
			ensure(state.Version == 1 && bytes.Equal(state.Target, target(fmt.Sprint(i), "native")), "benchmark ref changed after restart")
		}
		for shard := range shards {
			total += len(logs(a, fmt.Sprintf("bench-cas-%d/partition-%d", shards, shard), "native"))
		}
		ensure(total == n, "benchmark logs lost after restart")
	}
}
func faultRequest(a Adapter, key string) Request {
	return mixed("fault", key, key, read(a, "fault", "native", "main"), read(a, "fault", "git", "refs/tags/v1"))
}
func faultCheck(a Adapter, q Request) map[string]any {
	start := time.Now()
	first := apply(a, q, "lost-reply")
	ensure(first.Outcome == "Retry" || first.Outcome == "Replay", "failover did not preserve finite reply")
	second := apply(a, q, "")
	ensure(second.Outcome == "Replay", "failover did not replay the committed receipt")
	for i, kind := range []string{"native", "git"} {
		state := read(a, "fault", kind, string(q.Updates[i].Name))
		ensure(state.Version == q.Updates[i].Expected.Version+1 && bytes.Equal(state.Target, q.Updates[i].Target), "fault published wrong ref")
		ensure(len(logs(a, "fault", kind)) == int(state.Version), "fault doubled or lost a log")
	}
	return map[string]any{"elapsedMS": float64(time.Since(start).Microseconds()) / 1000, "outcome": second.Outcome, "exactlyOnePublication": true}
}
func run() error {
	backend := flag.String("backend", "tikv", "postgres or tikv")
	phase := flag.String("phase", "baseline", "baseline, one-node-down, pd-down, prepare-outage, outage, recover, verify")
	namespace := flag.String("namespace", "", "unique evaluation key namespace")
	output := flag.String("output", "", "JSON result file")
	input := flag.String("input", "", "previously prepared Request JSON")
	operations := flag.Int("operations", 256, "small-operation count per benchmark")
	flag.Parse()
	if !regexp.MustCompile(`^[a-z0-9-]{1,64}$`).MatchString(*namespace) || *operations < 8 || *operations > 4096 {
		return fmt.Errorf("invalid namespace or operation count")
	}
	logger, properties, err := pinglog.InitLogger(&pinglog.Config{Level: "error"})
	if err != nil {
		return err
	}
	pinglog.ReplaceGlobals(logger, properties)
	var store Store
	if *backend == "postgres" {
		ctx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
		store, err = newPostgres(ctx, os.Getenv("KASU_EVAL_POSTGRES_DSN"))
		cancel()
	} else if *backend == "tikv" {
		store, err = newTiKV(strings.Split(os.Getenv("KASU_EVAL_PD"), ","))
	} else {
		return fmt.Errorf("unknown backend")
	}
	if err != nil {
		return err
	}
	defer store.Close()
	a := Adapter{Store: store, Root: "kasu-kv-eval/v1/" + *namespace}
	result := map[string]any{"schema": 1, "backend": store.Name(), "phase": *phase, "namespace": *namespace, "operations": *operations}
	switch *phase {
	case "baseline":
		if *backend == "tikv" {
			result["regions"] = prepareRegions(a)
		}
		result["contracts"] = contracts(a)
		result["isolationProbe"] = isolationProbe(a)
		result["benchmarks"] = benchmarks(a, *operations)
		result["valueLimits"] = valueLimits(a)
		ensure(apply(a, mixed("fault", "base", "base", State{}, State{}), "").Outcome == "Applied", "failed to initialize fault markers")
		verify(a, *operations)
		if *backend == "tikv" {
			result["faultRegion"] = topology(store.(*tikvStore), a.refKey([]byte("fault"), "native", []byte("main")))
		}
		digest, count, err := stableDigest(a)
		checked(err)
		result["stableDataDigest"] = digest
		result["stableDataRecords"] = count
		result["allChecksPassed"] = true
	case "one-node-down", "pd-down":
		result["fault"] = faultCheck(a, faultRequest(a, *phase))
		result["allChecksPassed"] = true
	case "prepare-outage":
		result["request"] = faultRequest(a, "majority-loss")
		result["allChecksPassed"] = true
	case "outage", "recover":
		raw, err := os.ReadFile(*input)
		if err != nil {
			return err
		}
		var prepared struct {
			Request Request `json:"request"`
		}
		if err = json.Unmarshal(raw, &prepared); err != nil {
			return err
		}
		ensure(len(prepared.Request.Key) > 0, "prepared request missing")
		if *phase == "outage" {
			ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
			start := time.Now()
			response, err := a.Apply(ctx, prepared.Request, "")
			cancel()
			elapsed := time.Since(start)
			ensure(response.Outcome != "Applied" && response.Outcome != "Replay", "minority acknowledged a write")
			ensure(elapsed < 5*time.Second, "minority request failed to terminate within deadline budget")
			result["elapsedMS"] = float64(elapsed.Microseconds()) / 1000
			result["outcome"] = response.Outcome
			result["errorType"] = fmt.Sprintf("%T", err)
			result["noAcknowledgedWrite"] = true
		} else {
			var response Result
			var replies []map[string]any
			for budget := 0; budget < 10; budget++ {
				start := time.Now()
				response = apply(a, prepared.Request, "")
				reply := map[string]any{"outcome": response.Outcome, "attempts": response.Attempts, "elapsedMS": float64(time.Since(start).Microseconds()) / 1000}
				replies = append(replies, reply)
				fmt.Fprintln(os.Stderr, "recovery budget", budget+1, "outcome", response.Outcome, "attempts", response.Attempts)
				if response.Outcome != "Retry" {
					break
				}
				time.Sleep(100 * time.Millisecond)
			}
			result["recoveryReplies"] = replies
			ensure(response.Outcome == "Applied" || response.Outcome == "Replay", fmt.Sprintf("quorum recovery did not finish original key: %+v", response))
			response = apply(a, prepared.Request, "")
			ensure(response.Outcome == "Replay", "quorum recovery double publication")
			for _, u := range prepared.Request.Updates {
				state := read(a, string(prepared.Request.Repo), u.Kind, string(u.Name))
				ensure(state.Version == u.Expected.Version+1 && bytes.Equal(state.Target, u.Target), "recovered publication has wrong ref")
				ensure(len(logs(a, string(prepared.Request.Repo), u.Kind)) == int(state.Version), "recovery lost or doubled logs")
			}
			result["outcome"] = response.Outcome
			result["exactlyOnePublication"] = true
		}
		result["allChecksPassed"] = true
	case "verify", "verify-baseline":
		verify(a, *operations)
		expected := 4
		if *phase == "verify-baseline" {
			expected = 1
		}
		faultAudit(a, expected)
		raw, err := os.ReadFile(*input)
		checked(err)
		var baseline struct {
			Digest  string `json:"stableDataDigest"`
			Records int    `json:"stableDataRecords"`
		}
		checked(json.Unmarshal(raw, &baseline))
		digest, count, err := stableDigest(a)
		checked(err)
		ensure(digest == baseline.Digest && count == baseline.Records && count > 0, "complete stable data digest changed after restart")
		result["stableDataDigest"] = digest
		result["stableDataRecords"] = count
		result["allChecksPassed"] = true
	default:
		return fmt.Errorf("unknown phase")
	}
	raw, err := json.MarshalIndent(result, "", "  ")
	if err != nil {
		return err
	}
	if *output != "" {
		return os.WriteFile(*output, append(raw, '\n'), 0644)
	}
	fmt.Println(string(raw))
	return nil
}
func main() {
	if err := run(); err != nil {
		fmt.Fprintln(os.Stderr, "evaluation failed:", err)
		os.Exit(1)
	}
}
