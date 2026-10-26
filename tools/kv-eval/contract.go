package main

import (
	"bytes"
	"context"
	"errors"
	"fmt"
	"sync"
	"time"
)

func ensure(ok bool, message string) {
	if !ok {
		panic(message)
	}
}
func checked(err error) {
	if err != nil {
		panic(err)
	}
}
func opContext() (context.Context, context.CancelFunc) {
	return context.WithTimeout(context.Background(), 3*time.Second)
}
func apply(a Adapter, q Request, fault string) Result {
	ctx, cancel := opContext()
	defer cancel()
	r, err := a.Apply(ctx, q, fault)
	checked(err)
	return r
}
func read(a Adapter, repo, kind, name string) State {
	ctx, cancel := opContext()
	defer cancel()
	state, err := a.ReadRef(ctx, []byte(repo), kind, []byte(name))
	checked(err)
	return state
}
func logs(a Adapter, repo, kind string) []Log {
	ctx, cancel := opContext()
	defer cancel()
	out, err := a.Logs(ctx, []byte(repo), kind)
	checked(err)
	for n, entry := range out {
		ensure(entry.Sequence == uint64(n+1), "non-contiguous or reordered feed")
	}
	return out
}
func seal(q Request) Request {
	q.Fingerprint = nil
	q.Fingerprint = target(string(encode(q)), "native")
	return q
}
func mixed(repo, key, label string, expectedNative, expectedGit State) Request {
	q := request(repo, key, "native", "main", label, expectedNative)
	q.Updates = append(q.Updates, Update{Kind: "git", Name: []byte("refs/tags/v1"), Expected: expectedGit, Target: target(label, "git")})
	return seal(q)
}
func race(n int, work func(int) Result) []Result {
	start := make(chan struct{})
	out := make([]Result, n)
	var wg sync.WaitGroup
	for i := range n {
		wg.Add(1)
		go func() { defer wg.Done(); <-start; out[i] = work(i) }()
	}
	close(start)
	wg.Wait()
	return out
}
func contracts(a Adapter) map[string]any {
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()
	longKey := append(bytes.Repeat([]byte{0xff}, 8192), 0)
	checked(a.Put(ctx, longKey, []byte{}))
	value, err := a.Get(ctx, longKey)
	checked(err)
	ensure(value != nil && len(value) == 0, "empty immutable value lost")
	checked(a.Put(ctx, longKey, []byte{}))
	ensure(errors.Is(a.Put(ctx, longKey, []byte("changed")), collision), "immutable overwrite accepted")
	results := race(16, func(n int) Result {
		return apply(a, mixed("mixed-race", fmt.Sprint(n), fmt.Sprint(n), State{}, State{}), "")
	})
	initialMixed := map[string]int{}
	for i, r := range results {
		initialMixed[r.Outcome]++
		if r.Outcome == "Retry" {
			results[i] = apply(a, mixed("mixed-race", fmt.Sprint(i), fmt.Sprint(i), State{}, State{}), "")
		}
	}
	applied, mismatched := 0, 0
	winner := -1
	for i, r := range results {
		switch r.Outcome {
		case "Applied", "Replay":
			applied++
			winner = i
		case "Mismatch":
			mismatched++
		default:
			panic("mixed CAS did not finish after a fresh budget: " + r.Outcome)
		}
	}
	ensure(applied == 1 && mismatched == 15, "mixed CAS did not have one winner")
	ensure(bytes.Equal(read(a, "mixed-race", "native", "main").Target, target(fmt.Sprint(winner), "native")), "native winner wrong")
	ensure(bytes.Equal(read(a, "mixed-race", "git", "refs/tags/v1").Target, target(fmt.Sprint(winner), "git")), "tag winner differs")
	ensure(len(logs(a, "mixed-race", "native")) == 1 && len(logs(a, "mixed-race", "git")) == 1, "mixed logs not atomic")
	receiptRequest := mixed("receipt-race", "same-key", "one", State{}, State{})
	receipts := race(8, func(int) Result { return apply(a, receiptRequest, "") })
	initialReceipts := map[string]int{}
	for i, r := range receipts {
		initialReceipts[r.Outcome]++
		if r.Outcome == "Retry" {
			receipts[i] = apply(a, receiptRequest, "")
		}
	}
	applied, replayed := 0, 0
	for _, r := range receipts {
		switch r.Outcome {
		case "Applied":
			applied++
		case "Replay":
			replayed++
		default:
			panic("same-key race did not finish after a fresh budget: " + r.Outcome)
		}
	}
	ensure(applied <= 1 && applied+replayed == 8, "duplicate publication under one key")
	ensure(len(logs(a, "receipt-race", "native")) == 1 && len(logs(a, "receipt-race", "git")) == 1, "receipt wrote duplicate logs")
	wrong := receiptRequest
	wrong.Fingerprint = []byte("wrong request")
	_, err = a.Apply(ctx, wrong, "")
	ensure(errors.Is(err, collision), "reused key accepted a new fingerprint")
	stale := mixed("mixed-race", "rejected", "not-published", State{}, State{})
	stale.Updates[0].Name = []byte("otherwise-valid")
	stale = seal(stale)
	ensure(apply(a, stale, "").Outcome == "Mismatch", "stale tag accepted")
	ensure(read(a, "mixed-race", "native", "otherwise-valid").Version == 0, "partial batch was published")
	aborted := mixed("rollback", "same-key", "one", State{}, State{})
	_, err = a.Apply(ctx, aborted, "rollback")
	ensure(errors.Is(err, injected), "rollback fault not injected")
	ensure(read(a, "rollback", "native", "main").Version == 0 && read(a, "rollback", "git", "refs/tags/v1").Version == 0, "rollback left refs")
	ensure(len(logs(a, "rollback", "native")) == 0 && len(logs(a, "rollback", "git")) == 0, "rollback left logs")
	ensure(apply(a, aborted, "").Outcome == "Applied", "rollback retained receipt or counters")
	ensure(len(logs(a, "rollback", "native")) == 1, "counter did not roll back")
	for _, kind := range []string{"native", "git"} {
		repo := "aba-" + kind
		name := "ref"
		first := apply(a, request(repo, "create", kind, name, "same", State{}), "").States[0]
		deleted := request(repo, "delete", kind, name, "", first)
		deleted.Updates[0].Target = nil
		deleted = seal(deleted)
		second := apply(a, deleted, "").States[0]
		ensure(second.Version == 2 && second.Target == nil, "delete lost tombstone")
		ensure(apply(a, request(repo, "stale-create", kind, name, "same", State{}), "").Outcome == "Mismatch", "ABA deletion reset version")
		third := apply(a, request(repo, "recreate", kind, name, "same", second), "").States[0]
		ensure(third.Version == 3 && bytes.Equal(third.Target, first.Target), "ABA recreate not set")
		ensure(apply(a, request(repo, "stale-update", kind, name, "new", first), "").Outcome == "Mismatch", "ABA same target bypassed version")
		ensure(len(logs(a, repo, kind)) == 3, "ABA changed log count")
	}
	lost := mixed("lost-reply", "once", "durable", State{}, State{})
	ensure(apply(a, lost, "lost-reply").Outcome == "Retry", "reply loss not reported Retry")
	ensure(apply(a, lost, "").Outcome == "Replay", "reply loss did not replay")
	ensure(len(logs(a, "lost-reply", "native")) == 1 && len(logs(a, "lost-reply", "git")) == 1, "reply-loss double publication")
	tx, err := a.Store.Begin(ctx)
	checked(err)
	cancelled, cancelNow := context.WithCancel(context.Background())
	checked(tx.Put([]byte(a.Root+"/cancelled"), []byte("must roll back")))
	cancelNow()
	ensure(tx.Commit(cancelled) != nil, "cancelled commit succeeded")
	rollback(tx)
	verification, err := a.Store.Begin(ctx)
	checked(err)
	raw, err := verification.Get(ctx, []byte(a.Root+"/cancelled"))
	checked(err)
	rollback(verification)
	ensure(raw == nil, "cancelled transaction published a write")
	return map[string]any{"initialMixedReplies": initialMixed, "initialReceiptReplies": initialReceipts, "mixedCasClients": 16, "singleMixedWinner": true, "receiptClients": 8, "sameKeyPublishedOnce": true, "fingerprintCollisionRejected": true, "partialBatchRejected": true, "rollbackIncludesCountersAndReceipt": true, "nativeAndGitABARejected": true, "lostReplyReplayedOnce": true, "cancelledCommitRolledBack": true, "longBinaryKeyAndEmptyValue": true}
}
