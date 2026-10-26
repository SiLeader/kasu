package main

import (
	"bytes"
	"context"
	"encoding/binary"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"github.com/zeebo/blake3"
	"time"
)

// Cover every committed record, including value-limit probes and receipts, not
// only a sample of benchmark keys. Fault records are checked against their exact
// expected sequence separately because the fault phases legitimately change them.
func stableDigest(a Adapter) (string, int, error) {
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()
	tx, err := a.Store.Begin(ctx)
	if err != nil {
		return "", 0, err
	}
	defer rollback(tx)
	prefix := []byte(a.Root + "/")
	end := append(append([]byte(nil), prefix...), 0xff)
	pairs, err := tx.Scan(ctx, prefix, end, 100000)
	if err != nil {
		return "", 0, err
	}
	if len(pairs) >= 100000 {
		return "", 0, errors.New("audit scan limit reached")
	}
	h := blake3.New()
	count := 0
	repo := digest([]byte("fault"))
	for _, pair := range pairs {
		skip := false
		for _, domain := range []string{"refs", "count", "logs", "once"} {
			if bytes.HasPrefix(pair.Key, []byte(a.Root+"/"+domain+"/"+repo+"/")) {
				skip = true
			}
		}
		if skip {
			continue
		}
		var size [8]byte
		binary.BigEndian.PutUint64(size[:], uint64(len(pair.Key)))
		_, _ = h.Write(size[:])
		_, _ = h.Write(pair.Key)
		binary.BigEndian.PutUint64(size[:], uint64(len(pair.Value)))
		_, _ = h.Write(size[:])
		_, _ = h.Write(pair.Value)
		count++
	}
	return hex.EncodeToString(h.Sum(nil)), count, nil
}
func faultAudit(a Adapter, expected int) {
	labels := []string{"base", "one-node-down", "pd-down", "majority-loss"}
	labels = labels[:expected]
	for _, kind := range []string{"native", "git"} {
		name := "main"
		if kind == "git" {
			name = "refs/tags/v1"
		}
		state := read(a, "fault", kind, name)
		ensure(state.Version == uint64(expected) && bytes.Equal(state.Target, target(labels[len(labels)-1], kind)), "fault final target changed after restart")
		entries := logs(a, "fault", kind)
		ensure(len(entries) == expected, "fault log count changed")
		var before State
		for n, label := range labels {
			entry := entries[n]
			after := State{Version: uint64(n + 1), Target: target(label, kind)}
			ensure(equal(entry.Before, before) && equal(entry.After, after) && bytes.Equal(entry.RequestKey, []byte(label)), "fault log content changed after restart")
			before = after
		}
	}
	ctx, cancel := opContext()
	defer cancel()
	tx, err := a.Store.Begin(ctx)
	checked(err)
	defer rollback(tx)
	var native, git State
	for n, label := range labels {
		q := mixed("fault", label, label, native, git)
		raw, err := tx.Get(ctx, a.receiptKey(q.Repo, q.Key))
		checked(err)
		var receipt Receipt
		checked(json.Unmarshal(raw, &receipt))
		ensure(bytes.Equal(receipt.Fingerprint, q.Fingerprint) && receipt.Result.Outcome == "Applied" && len(receipt.Result.States) == 2, "fault receipt changed after restart")
		native = State{Version: uint64(n + 1), Target: target(label, "native")}
		git = State{Version: uint64(n + 1), Target: target(label, "git")}
		ensure(equal(receipt.Result.States[0], native) && equal(receipt.Result.States[1], git), "fault receipt returned a different result")
	}
	for _, kind := range []string{"native", "git"} {
		raw, err := tx.Get(ctx, a.counterKey([]byte("fault"), kind))
		checked(err)
		ensure(len(raw) == 8 && binary.BigEndian.Uint64(raw) == uint64(expected), fmt.Sprintf("%s fault counter changed", kind))
	}
}
