package server

import (
	"context"
	"errors"
	"log/slog"
	"sync"

	"github.com/JayTsu-sh/robinhood-rs/agents/juicefs-changelog-agent/internal/cursor"
	changelogv1 "github.com/JayTsu-sh/robinhood-rs/agents/juicefs-changelog-agent/internal/gen/changelogv1"
	"github.com/JayTsu-sh/robinhood-rs/agents/juicefs-changelog-agent/internal/source"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"
)

type Changelog struct {
	changelogv1.UnimplementedChangelogServer
	sources   map[string]source.Scanner
	sem       chan struct{}
	log       *slog.Logger
	cursors   cursor.Store
	mu        sync.Mutex
	active    map[string]bool
	delivered map[string]int64
}

func New(sources map[string]source.Scanner, cursors cursor.Store, maxStreams uint32, logger *slog.Logger) *Changelog {
	return &Changelog{
		sources: sources, cursors: cursors, sem: make(chan struct{}, maxStreams), log: logger,
		active: make(map[string]bool), delivered: make(map[string]int64),
	}
}

func (s *Changelog) Watch(req *changelogv1.WatchRequest, stream changelogv1.Changelog_WatchServer) error {
	scanner, ok := s.sources[req.GetVolume()]
	if !ok {
		return status.Error(codes.NotFound, "volume is not configured")
	}
	select {
	case s.sem <- struct{}{}:
		defer func() { <-s.sem }()
	default:
		return status.Error(codes.ResourceExhausted, "maximum concurrent streams reached")
	}
	s.mu.Lock()
	if s.active[req.GetVolume()] {
		s.mu.Unlock()
		return status.Error(codes.AlreadyExists, "volume already has an active consumer")
	}
	s.active[req.GetVolume()] = true
	s.mu.Unlock()
	defer func() {
		s.mu.Lock()
		delete(s.active, req.GetVolume())
		s.mu.Unlock()
	}()

	after, err := s.cursors.Get(stream.Context(), req.GetVolume())
	if err != nil {
		return status.Error(codes.Internal, "cursor unavailable")
	}
	s.log.Info("changelog stream opened", "volume", req.GetVolume(), "after_version", after)
	err = scanner.Scan(stream.Context(), after, func(version int64, entry string) error {
		if err := stream.Send(&changelogv1.ChangelogRecord{
			Volume: req.GetVolume(), Version: version, Entry: entry,
		}); err != nil {
			return err
		}
		s.mu.Lock()
		if version > s.delivered[req.GetVolume()] {
			s.delivered[req.GetVolume()] = version
		}
		s.mu.Unlock()
		return nil
	})
	if errors.Is(err, context.Canceled) || errors.Is(err, context.DeadlineExceeded) {
		return nil
	}
	if err != nil {
		s.log.Error("changelog scan failed", "volume", req.GetVolume(), "error", err)
		return status.Error(codes.Unavailable, "changelog source unavailable")
	}
	return nil
}

func (s *Changelog) Ack(ctx context.Context, req *changelogv1.AckRequest) (*changelogv1.AckResponse, error) {
	if _, ok := s.sources[req.GetVolume()]; !ok {
		return nil, status.Error(codes.NotFound, "volume is not configured")
	}
	if req.GetVersion() <= 0 {
		return nil, status.Error(codes.InvalidArgument, "version must be positive")
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	if req.GetVersion() > s.delivered[req.GetVolume()] {
		return nil, status.Error(codes.FailedPrecondition, "version has not been delivered")
	}
	if err := s.cursors.Commit(ctx, req.GetVolume(), req.GetVersion()); err != nil {
		s.log.Error("cursor commit failed", "volume", req.GetVolume(), "error", err)
		return nil, status.Error(codes.Internal, "cursor commit failed")
	}
	return &changelogv1.AckResponse{}, nil
}
