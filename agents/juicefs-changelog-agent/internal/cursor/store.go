package cursor

import "context"

type Store interface {
	Get(context.Context, string) (int64, error)
	Commit(context.Context, string, int64) error
}
