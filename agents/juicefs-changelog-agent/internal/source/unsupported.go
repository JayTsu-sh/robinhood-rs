//go:build !fdb

package source

import "fmt"

func NewJuiceFS(string) (*unsupportedJuiceFS, error) {
	return nil, fmt.Errorf("agent must be built with -tags fdb")
}

type unsupportedJuiceFS struct{}
