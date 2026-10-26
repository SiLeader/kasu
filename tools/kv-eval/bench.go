package main

import (
	"bytes"
	"context"
	"errors"
	"fmt"
	tikverr "github.com/tikv/client-go/v2/error"
	"sort"
	"strings"
	"sync"
	"time"
)

type Timing struct {
	Name                  string
	Operations            int
	Concurrency           int
	Applied               int
	Retry                 int
	Attempts              int
	ElapsedMS             float64
	AcknowledgedPerSecond float64
	P50MS                 float64
	P95MS                 float64
	P99MS                 float64
	DrainMS               float64
}

func benchmark(name string, n, concurrency int, op func(int) Result, drain func(int)) Timing {
	start := make(chan struct{})
	jobs := make(chan int, n)
	for i := range n {
		jobs <- i
	}
	close(jobs)
	durations := make([]float64, n)
	results := make([]Result, n)
	var wg sync.WaitGroup
	begin := time.Now()
	for range concurrency {
		wg.Add(1)
		go func() {
			defer wg.Done()
			<-start
			for i := range jobs {
				t := time.Now()
				results[i] = op(i)
				durations[i] = float64(time.Since(t).Microseconds()) / 1000
			}
		}()
	}
	close(start)
	wg.Wait()
	elapsed := time.Since(begin)
	timing := Timing{Name: name, Operations: n, Concurrency: concurrency, ElapsedMS: float64(elapsed.Microseconds()) / 1000}
	for _, r := range results {
		timing.Attempts += r.Attempts
		switch r.Outcome {
		case "Applied", "Replay":
			timing.Applied++
		case "Retry":
			timing.Retry++
		default:
			panic("unexpected benchmark outcome: " + r.Outcome)
		}
	}
	begin = time.Now()
	for i, r := range results {
		if r.Outcome == "Retry" {
			if drain == nil {
				panic("unhandled benchmark Retry")
			}
			drain(i)
		}
	}
	timing.DrainMS = float64(time.Since(begin).Microseconds()) / 1000
	sort.Float64s(durations)
	at := func(p float64) float64 { return durations[int(p*float64(n-1))] }
	timing.P50MS = at(.50)
	timing.P95MS = at(.95)
	timing.P99MS = at(.99)
	timing.AcknowledgedPerSecond = float64(timing.Applied) / elapsed.Seconds()
	return timing
}
func benchmarks(a Adapter, n int) []Timing {
	value := make([]byte, 4096)
	for i := range value {
		value[i] = byte((i*131 + 17) % 251)
	}
	results := []Timing{benchmark("immutable-put-4KiB", n, 1, func(i int) Result {
		ctx, cancel := opContext()
		defer cancel()
		checked(a.Put(ctx, []byte(fmt.Sprintf("bench/object/%d", i)), value))
		return Result{Outcome: "Applied", Attempts: 1}
	}, nil)}
	results = append(results, benchmark("immutable-get-4KiB", n, 1, func(i int) Result {
		ctx, cancel := opContext()
		defer cancel()
		got, err := a.Get(ctx, []byte(fmt.Sprintf("bench/object/%d", i)))
		checked(err)
		ensure(bytes.Equal(got, value), "immutable benchmark read differs")
		return Result{Outcome: "Applied", Attempts: 1}
	}, nil))
	for _, shards := range []int{1, 8} {
		requests := make([]Request, n)
		for i := range n {
			repo := fmt.Sprintf("bench-cas-%d/partition-%d", shards, i%shards)
			requests[i] = request(repo, fmt.Sprint(i), "native", fmt.Sprintf("ref-%d", i), fmt.Sprint(i), State{})
		}
		timing := benchmark(fmt.Sprintf("ref-cas-%d-repos", shards), n, 8, func(i int) Result { return apply(a, requests[i], "") }, func(i int) {
			r := apply(a, requests[i], "")
			ensure(r.Outcome == "Applied" || r.Outcome == "Replay", "sequential drain did not finish")
		})
		for i, q := range requests {
			state := read(a, string(q.Repo), "native", string(q.Updates[0].Name))
			ensure(state.Version == 1 && bytes.Equal(state.Target, target(fmt.Sprint(i), "native")), "acknowledged ref missing after concurrent benchmark")
		}
		total := 0
		for shard := range shards {
			total += len(logs(a, fmt.Sprintf("bench-cas-%d/partition-%d", shards, shard), "native"))
		}
		ensure(total == n, "benchmark lost or doubled a log")
		results = append(results, timing)
	}
	return results
}
func valueLimits(a Adapter) []map[string]any {
	var results []map[string]any
	for _, size := range []int{1024, 1024 * 1024, 8 * 1024 * 1024} {
		ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
		value := bytes.Repeat([]byte{0x63}, size)
		key := []byte(fmt.Sprintf("limit/%d", size))
		start := time.Now()
		err := a.Put(ctx, key, value)
		class := ""
		if err != nil {
			var tooLarge *tikverr.ErrEntryTooLarge
			if errors.As(err, &tooLarge) {
				class = "entry-too-large"
			} else if strings.Contains(err.Error(), "raft entry is too large") {
				class = "raft-entry-too-large"
			} else {
				cancel()
				panic(fmt.Sprintf("unexpected value-limit failure: %T %v", err, err))
			}
			got, e := a.Get(ctx, key)
			checked(e)
			ensure(got == nil, "rejected large value was published")
		} else {
			got, e := a.Get(ctx, key)
			checked(e)
			ensure(bytes.Equal(got, value), "accepted large value changed")
		}
		results = append(results, map[string]any{"rawBytes": size, "accepted": err == nil, "errorClass": class, "elapsedMS": float64(time.Since(start).Microseconds()) / 1000})
		cancel()
	}
	return results
}
