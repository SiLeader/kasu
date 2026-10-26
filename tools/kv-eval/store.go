package main

import (
	"context"
	"errors"
	"time"

	"github.com/jackc/pgx/v5"
	"github.com/jackc/pgx/v5/pgconn"
	"github.com/jackc/pgx/v5/pgxpool"
	tikverr "github.com/tikv/client-go/v2/error"
	"github.com/tikv/client-go/v2/tikv"
	"github.com/tikv/client-go/v2/txnkv"
	"github.com/tikv/client-go/v2/txnkv/transaction"
)

type Pair struct{ Key, Value []byte }
type Transaction interface {
	Get(context.Context, []byte) ([]byte, error)
	Put([]byte, []byte) error
	Scan(context.Context, []byte, []byte, int) ([]Pair, error)
	Commit(context.Context) error
	Rollback(context.Context) error
}
type Store interface {
	Begin(context.Context) (Transaction, error)
	Close()
	Name() string
}
type pgStore struct{ pool *pgxpool.Pool }

func newPostgres(ctx context.Context, dsn string) (*pgStore, error) {
	config, err := pgxpool.ParseConfig(dsn)
	if err != nil {
		return nil, err
	}
	config.MaxConns = 32
	config.ConnConfig.ConnectTimeout = 5 * time.Second
	config.ConnConfig.RuntimeParams["statement_timeout"] = "5000"
	config.ConnConfig.RuntimeParams["lock_timeout"] = "3000"
	config.ConnConfig.RuntimeParams["synchronous_commit"] = "on"
	pool, err := pgxpool.NewWithConfig(ctx, config)
	if err != nil {
		return nil, err
	}
	_, err = pool.Exec(ctx, "CREATE TABLE IF NOT EXISTS kasu_kv_evaluation (key BYTEA PRIMARY KEY, value BYTEA NOT NULL)")
	if err != nil {
		pool.Close()
		return nil, err
	}
	return &pgStore{pool}, nil
}
func (s *pgStore) Name() string { return "PostgreSQL serializable KV prototype" }
func (s *pgStore) Close()       { s.pool.Close() }
func (s *pgStore) Begin(ctx context.Context) (Transaction, error) {
	tx, err := s.pool.BeginTx(ctx, pgx.TxOptions{IsoLevel: pgx.Serializable})
	if err != nil {
		return nil, err
	}
	return &pgTransaction{tx: tx, ctx: ctx}, nil
}

type pgTransaction struct {
	tx  pgx.Tx
	ctx context.Context
}

func (t *pgTransaction) Get(ctx context.Context, key []byte) ([]byte, error) {
	var value []byte
	err := t.tx.QueryRow(ctx, "SELECT value FROM kasu_kv_evaluation WHERE key=$1", key).Scan(&value)
	if errors.Is(err, pgx.ErrNoRows) {
		return nil, nil
	}
	return value, err
}
func (t *pgTransaction) Put(key, value []byte) error {
	_, err := t.tx.Exec(t.ctx, "INSERT INTO kasu_kv_evaluation(key,value) VALUES ($1,$2) ON CONFLICT(key) DO UPDATE SET value=excluded.value", key, value)
	return err
}
func (t *pgTransaction) Scan(ctx context.Context, start, end []byte, limit int) ([]Pair, error) {
	rows, err := t.tx.Query(ctx, "SELECT key,value FROM kasu_kv_evaluation WHERE key >= $1 AND key < $2 ORDER BY key LIMIT $3", start, end, limit)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	var out []Pair
	for rows.Next() {
		var pair Pair
		if err = rows.Scan(&pair.Key, &pair.Value); err != nil {
			return nil, err
		}
		out = append(out, pair)
	}
	return out, rows.Err()
}
func (t *pgTransaction) Commit(ctx context.Context) error   { return t.tx.Commit(ctx) }
func (t *pgTransaction) Rollback(ctx context.Context) error { return t.tx.Rollback(ctx) }

type tikvStore struct{ client *txnkv.Client }

func newTiKV(endpoints []string) (*tikvStore, error) {
	client, err := txnkv.NewClient(endpoints)
	if err != nil {
		return nil, err
	}
	return &tikvStore{client}, nil
}
func (s *tikvStore) Name() string { return "TiKV optimistic TxnKV prototype" }
func (s *tikvStore) Close()       { _ = s.client.Close() }
func (s *tikvStore) Begin(ctx context.Context) (Transaction, error) {
	if err := ctx.Err(); err != nil {
		return nil, err
	}
	ts, err := s.client.GetTimestamp(ctx)
	if err != nil {
		return nil, err
	}
	tx, err := s.client.Begin(tikv.WithStartTS(ts))
	if err != nil {
		return nil, err
	}
	tx.SetEnable1PC(false)
	tx.SetEnableAsyncCommit(false)
	return &tikvTransaction{tx}, nil
}

type tikvTransaction struct{ tx *transaction.KVTxn }

func (t *tikvTransaction) Get(ctx context.Context, key []byte) ([]byte, error) {
	value, err := t.tx.Get(ctx, key)
	if tikverr.IsErrNotFound(err) {
		return nil, nil
	}
	return value.Value, err
}
func (t *tikvTransaction) Put(key, value []byte) error { return t.tx.Set(key, value) }
func (t *tikvTransaction) Scan(ctx context.Context, start, end []byte, limit int) ([]Pair, error) {
	if err := ctx.Err(); err != nil {
		return nil, err
	}
	it, err := t.tx.Iter(start, end)
	if err != nil {
		return nil, err
	}
	defer it.Close()
	var out []Pair
	for it.Valid() && len(out) < limit {
		if err := ctx.Err(); err != nil {
			return nil, err
		}
		out = append(out, Pair{append([]byte(nil), it.Key()...), append([]byte(nil), it.Value()...)})
		if err = it.Next(); err != nil {
			return nil, err
		}
	}
	return out, nil
}
func (t *tikvTransaction) Commit(ctx context.Context) error { return t.tx.Commit(ctx) }
func (t *tikvTransaction) Rollback(context.Context) error   { return t.tx.Rollback() }
func conflict(err error) bool {
	var retryable *tikverr.ErrRetryable
	var pg *pgconn.PgError
	return errors.As(err, &pg) && (pg.Code == "40001" || pg.Code == "40P01") || tikverr.IsErrWriteConflict(err) || errors.As(err, &retryable)
}
