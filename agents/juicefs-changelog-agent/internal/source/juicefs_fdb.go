//go:build fdb

package source

import (
	"context"
	"fmt"

	"github.com/juicedata/juicefs/pkg/meta"
)

type JuiceFS struct {
	client meta.Meta
}

func NewJuiceFS(metaURL string) (*JuiceFS, error) {
	client := meta.NewClient(metaURL, nil)
	format, err := client.Load(true)
	if err != nil {
		return nil, fmt.Errorf("load JuiceFS volume: %w", err)
	}
	if !format.ChangeLog {
		return nil, fmt.Errorf("JuiceFS changelog is disabled")
	}
	return &JuiceFS{client: client}, nil
}

func (j *JuiceFS) Scan(ctx context.Context, after int64, emit func(int64, string) error) error {
	metaCtx := meta.WrapWithCancel(ctx, 0, 0, nil)
	defer metaCtx.Cancel()
	return j.client.ScanChangelog(metaCtx, after, filterAfter(after, emit))
}
