package source

import "context"

// Scanner is the internal seam around JuiceFS metadata changelog scanning.
// Implementations emit records strictly after the supplied durable cursor.
type Scanner interface {
	Scan(context.Context, int64, func(version int64, entry string) error) error
}

// FoundationDB uses JuiceFS's TKV implementation, which deliberately rewinds
// before the requested version. Give the gRPC interface strict "after" cursor
// semantics by suppressing records already durably acknowledged by the Agent.
func filterAfter(after int64, emit func(int64, string) error) func(int64, string) error {
	return func(version int64, entry string) error {
		if version <= after {
			return nil
		}
		return emit(version, entry)
	}
}
