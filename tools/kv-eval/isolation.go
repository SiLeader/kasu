package main

import (
	"context"
	"fmt"
	"time"
)

// A checked terminal Land result can read a ref without moving it. Exercise the
// read dependency explicitly: A reads v1; B commits v2; C sees v2 and no result;
// A then tries to publish that result. A strict serialization cannot put A on
// either side of both B and C. A same-value ref write is the TxnKV read fence.
func isolationProbe(a Adapter) map[string]any {
	result := map[string]any{}
	for _, fence := range []bool{false, true} {
		if _, postgres := a.Store.(*pgStore); postgres && fence {
			result["sameValueFence"] = "not-run: PostgreSQL writes acquire a row lock"
			continue
		}
		repo := fmt.Sprintf("guard-%t", fence)
		seed := apply(a, request(repo, "seed", "native", "main", "one", State{}), "").States[0]
		ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
		defer cancel()
		txA, err := a.Store.Begin(ctx)
		checked(err)
		defer rollback(txA)
		observed, err := readState(ctx, txA, a.refKey([]byte(repo), "native", []byte("main")))
		checked(err)
		ensure(equal(observed, seed), "guard did not read v1")
		receiptKey := a.receiptKey([]byte(repo), []byte("terminal-result"))
		checked(txA.Put(receiptKey, []byte("checked terminal result")))
		if fence {
			checked(txA.Put(a.refKey([]byte(repo), "native", []byte("main")), encode(seed)))
		}
		txB, err := a.Store.Begin(ctx)
		checked(err)
		defer rollback(txB)
		current, err := readState(ctx, txB, a.refKey([]byte(repo), "native", []byte("main")))
		checked(err)
		ensure(equal(current, seed), "concurrent writer did not observe v1")
		checked(txB.Put(a.refKey([]byte(repo), "native", []byte("main")), encode(State{Target: target("two", "native"), Version: 2})))
		checked(txB.Commit(ctx))
		rollback(txB)
		txC, err := a.Store.Begin(ctx)
		checked(err)
		defer rollback(txC)
		state, err := readState(ctx, txC, a.refKey([]byte(repo), "native", []byte("main")))
		checked(err)
		receipt, err := txC.Get(ctx, receiptKey)
		checked(err)
		ensure(state.Version == 2 && receipt == nil, "observer did not see v2 without A's result")
		checked(txC.Commit(ctx))
		rollback(txC)
		err = txA.Commit(ctx)
		rollback(txA)
		cancel()
		if err != nil && !conflict(err) {
			panic(err)
		}
		if fence {
			ensure(err != nil, "explicit ref read fence failed to reject stale result")
			result["sameValueRefFenceAbortsStaleGuard"] = true
		} else {
			result["unfencedGuardCommittedAfterObserver"] = err == nil
			result["unfencedGuardAbortedAsConflict"] = err != nil
		}
	}
	return result
}
