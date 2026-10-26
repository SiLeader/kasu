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
	"math"
	"time"
)

var collision = errors.New("immutable key or request fingerprint collision")
var injected = errors.New("injected rollback before commit")
var lostReply = errors.New("injected reply loss after committed transaction")

type Ref struct {
	Kind  string
	Name  []byte
	State State
}
type State struct {
	Target  []byte
	Version uint64
}
type Update struct {
	Kind     string
	Name     []byte
	Expected State
	Target   []byte
}
type Request struct {
	Repo        []byte
	Key         []byte
	Fingerprint []byte
	Updates     []Update
}
type Result struct {
	Outcome  string
	States   []State
	Attempts int
}
type Receipt struct {
	Fingerprint []byte
	Result      Result
}
type Log struct {
	Sequence   uint64
	Kind       string
	Name       []byte
	Before     State
	After      State
	RequestKey []byte
}
type Adapter struct {
	Store Store
	Root  string
}

func digest(value []byte) string { sum := blake3.Sum256(value); return hex.EncodeToString(sum[:]) }
func target(value string, kind string) []byte {
	sum := blake3.Sum256([]byte(value))
	if kind == "git" {
		return sum[:20]
	}
	return sum[:]
}
func encode(value any) []byte {
	bytes, err := json.Marshal(value)
	if err != nil {
		panic(err)
	}
	return bytes
}
func rollback(tx Transaction) {
	ctx, cancel := context.WithTimeout(context.Background(), time.Second)
	defer cancel()
	_ = tx.Rollback(ctx)
}
func (a Adapter) refKey(repo []byte, kind string, name []byte) []byte {
	return []byte(a.Root + "/refs/" + digest(repo) + "/" + kind + "/" + digest(name))
}
func (a Adapter) logPrefix(repo []byte, kind string) []byte {
	return []byte(a.Root + "/logs/" + digest(repo) + "/" + kind + "/")
}
func (a Adapter) counterKey(repo []byte, kind string) []byte {
	return []byte(a.Root + "/count/" + digest(repo) + "/" + kind)
}
func (a Adapter) receiptKey(repo, key []byte) []byte {
	return []byte(a.Root + "/once/" + digest(repo) + "/" + digest(key))
}
func equal(a, b State) bool { return a.Version == b.Version && bytes.Equal(a.Target, b.Target) }
func readState(ctx context.Context, tx Transaction, key []byte) (State, error) {
	raw, err := tx.Get(ctx, key)
	if err != nil {
		return State{}, err
	}
	if raw == nil {
		return State{}, nil
	}
	var state State
	err = json.Unmarshal(raw, &state)
	return state, err
}
func (a Adapter) ReadRef(ctx context.Context, repo []byte, kind string, name []byte) (State, error) {
	tx, err := a.Store.Begin(ctx)
	if err != nil {
		return State{}, err
	}
	defer rollback(tx)
	return readState(ctx, tx, a.refKey(repo, kind, name))
}
func (a Adapter) Logs(ctx context.Context, repo []byte, kind string) ([]Log, error) {
	tx, err := a.Store.Begin(ctx)
	if err != nil {
		return nil, err
	}
	defer rollback(tx)
	prefix := a.logPrefix(repo, kind)
	end := append(append([]byte(nil), prefix...), 0xff)
	pairs, err := tx.Scan(ctx, prefix, end, 100000)
	if err != nil {
		return nil, err
	}
	out := make([]Log, 0, len(pairs))
	for _, pair := range pairs {
		var log Log
		if err = json.Unmarshal(pair.Value, &log); err != nil {
			return nil, err
		}
		out = append(out, log)
	}
	return out, nil
}
func (a Adapter) Apply(ctx context.Context, q Request, fault string) (Result, error) {
	if len(q.Key) == 0 || len(q.Updates) == 0 {
		return Result{}, errors.New("empty request")
	}
	names := map[string]bool{}
	for _, u := range q.Updates {
		key := string(a.refKey(q.Repo, u.Kind, u.Name))
		if names[key] {
			return Result{}, errors.New("duplicate update")
		}
		names[key] = true
	}
	for attempt := 1; attempt <= 4; attempt++ {
		if err := ctx.Err(); err != nil {
			return Result{Outcome: "Retry", Attempts: attempt}, nil
		}
		result, err := a.applyOnce(ctx, q, fault)
		result.Attempts = attempt
		if err == nil {
			return result, nil
		}
		if errors.Is(err, lostReply) {
			return Result{Outcome: "Retry", Attempts: attempt}, nil
		}
		if ctx.Err() != nil {
			return Result{Outcome: "Retry", Attempts: attempt}, nil
		}
		if !conflict(err) {
			return Result{}, err
		}
		timer := time.NewTimer(time.Duration(1<<attempt) * time.Millisecond)
		select {
		case <-ctx.Done():
			timer.Stop()
			return Result{Outcome: "Retry", Attempts: attempt}, nil
		case <-timer.C:
		}
	}
	return Result{Outcome: "Retry", Attempts: 4}, nil
}
func (a Adapter) applyOnce(ctx context.Context, q Request, fault string) (Result, error) {
	tx, err := a.Store.Begin(ctx)
	if err != nil {
		return Result{}, err
	}
	defer rollback(tx)
	receiptKey := a.receiptKey(q.Repo, q.Key)
	raw, err := tx.Get(ctx, receiptKey)
	if err != nil {
		return Result{}, err
	}
	if raw != nil {
		var receipt Receipt
		if err = json.Unmarshal(raw, &receipt); err != nil {
			return Result{}, err
		}
		if !bytes.Equal(receipt.Fingerprint, q.Fingerprint) {
			return Result{}, collision
		}
		result := receipt.Result
		result.Outcome = "Replay"
		return result, nil
	}
	states := make([]State, 0, len(q.Updates))
	for _, update := range q.Updates {
		actual, err := readState(ctx, tx, a.refKey(q.Repo, update.Kind, update.Name))
		if err != nil {
			return Result{}, err
		}
		if !equal(actual, update.Expected) {
			return Result{Outcome: "Mismatch", States: []State{actual}}, nil
		}
		if actual.Version == math.MaxUint64 {
			return Result{}, errors.New("version exhausted")
		}
		states = append(states, State{Target: update.Target, Version: actual.Version + 1})
	}
	counters := map[string]uint64{}
	for n, update := range q.Updates {
		if _, ok := counters[update.Kind]; !ok {
			raw, err := tx.Get(ctx, a.counterKey(q.Repo, update.Kind))
			if err != nil {
				return Result{}, err
			}
			if raw != nil {
				if len(raw) != 8 {
					return Result{}, errors.New("corrupt counter")
				}
				counters[update.Kind] = binary.BigEndian.Uint64(raw)
			}
		}
		if counters[update.Kind] == math.MaxUint64 {
			return Result{}, errors.New("log exhausted")
		}
		counters[update.Kind]++
		if err = tx.Put(a.refKey(q.Repo, update.Kind, update.Name), encode(states[n])); err != nil {
			return Result{}, err
		}
		log := Log{Sequence: counters[update.Kind], Kind: update.Kind, Name: update.Name, Before: update.Expected, After: states[n], RequestKey: q.Key}
		logKey := append(a.logPrefix(q.Repo, update.Kind), []byte(fmt.Sprintf("%016x", log.Sequence))...)
		if err = tx.Put(logKey, encode(log)); err != nil {
			return Result{}, err
		}
	}
	for kind, value := range counters {
		var raw [8]byte
		binary.BigEndian.PutUint64(raw[:], value)
		if err = tx.Put(a.counterKey(q.Repo, kind), raw[:]); err != nil {
			return Result{}, err
		}
	}
	result := Result{Outcome: "Applied", States: states}
	if err = tx.Put(receiptKey, encode(Receipt{Fingerprint: q.Fingerprint, Result: result})); err != nil {
		return Result{}, err
	}
	if fault == "rollback" {
		return Result{}, injected
	}
	if err = ctx.Err(); err != nil {
		return Result{}, err
	}
	if err = tx.Commit(ctx); err != nil {
		return Result{}, err
	}
	if fault == "lost-reply" {
		return Result{}, lostReply
	}
	return result, nil
}

// Immutable keys retain their original bytes behind a fixed-size index. Empty
// values need an envelope because TxnKV Set does not accept an empty value.
func immutable(key, value []byte) []byte {
	out := make([]byte, 8+len(key)+len(value))
	binary.BigEndian.PutUint64(out, uint64(len(key)))
	copy(out[8:], key)
	copy(out[8+len(key):], value)
	return out
}
func decodeImmutable(raw, key []byte) ([]byte, error) {
	if len(raw) < 8 {
		return nil, errors.New("short immutable envelope")
	}
	n := binary.BigEndian.Uint64(raw)
	if n > uint64(len(raw)-8) || !bytes.Equal(raw[8:8+n], key) {
		return nil, collision
	}
	return raw[8+n:], nil
}
func (a Adapter) Put(ctx context.Context, key, value []byte) error {
	for n := 0; n < 4; n++ {
		err := func() error {
			tx, err := a.Store.Begin(ctx)
			if err != nil {
				return err
			}
			defer rollback(tx)
			index := []byte(a.Root + "/meta/" + digest(key))
			raw, err := tx.Get(ctx, index)
			if err != nil {
				return err
			}
			if raw != nil {
				existing, err := decodeImmutable(raw, key)
				if err != nil {
					return err
				}
				if !bytes.Equal(existing, value) {
					return collision
				}
				return nil
			}
			if err = tx.Put(index, immutable(key, value)); err != nil {
				return err
			}
			return tx.Commit(ctx)
		}()
		if !conflict(err) {
			return err
		}
		if ctx.Err() != nil {
			return ctx.Err()
		}
	}
	return errors.New("immutable retry budget exhausted")
}
func (a Adapter) Get(ctx context.Context, key []byte) ([]byte, error) {
	tx, err := a.Store.Begin(ctx)
	if err != nil {
		return nil, err
	}
	defer rollback(tx)
	raw, err := tx.Get(ctx, []byte(a.Root+"/meta/"+digest(key)))
	if err != nil || raw == nil {
		return nil, err
	}
	return decodeImmutable(raw, key)
}
func request(repo, key, kind, name, value string, expected State) Request {
	r := Request{Repo: []byte(repo), Key: []byte(key), Updates: []Update{{Kind: kind, Name: []byte(name), Expected: expected, Target: target(value, kind)}}}
	sum := blake3.Sum256(encode(r))
	r.Fingerprint = sum[:]
	return r
}
