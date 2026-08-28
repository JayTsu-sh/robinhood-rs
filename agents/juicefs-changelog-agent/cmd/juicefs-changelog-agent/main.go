//go:build fdb

package main

import (
	"flag"
	"fmt"
	"log/slog"
	"net"
	"os"
	"os/signal"
	"syscall"

	"github.com/JayTsu-sh/robinhood-rs/agents/juicefs-changelog-agent/internal/config"
	"github.com/JayTsu-sh/robinhood-rs/agents/juicefs-changelog-agent/internal/cursor"
	changelogv1 "github.com/JayTsu-sh/robinhood-rs/agents/juicefs-changelog-agent/internal/gen/changelogv1"
	"github.com/JayTsu-sh/robinhood-rs/agents/juicefs-changelog-agent/internal/server"
	"github.com/JayTsu-sh/robinhood-rs/agents/juicefs-changelog-agent/internal/source"
	"google.golang.org/grpc"
)

func main() {
	configPath := flag.String("config", "/etc/juicefs-changelog-agent/config.yaml", "path to configuration")
	flag.Parse()

	logger := slog.New(slog.NewJSONHandler(os.Stdout, nil))
	if err := run(*configPath, logger); err != nil {
		logger.Error("agent stopped", "error", err)
		os.Exit(1)
	}
}

func run(configPath string, logger *slog.Logger) error {
	cfg, err := config.Load(configPath)
	if err != nil {
		return fmt.Errorf("load config: %w", err)
	}
	cursors, err := cursor.NewFileStore(cfg.CursorDir)
	if err != nil {
		return fmt.Errorf("initialize cursor store: %w", err)
	}

	sources := make(map[string]source.Scanner, len(cfg.Volumes))
	for name, volume := range cfg.Volumes {
		s, err := source.NewJuiceFS(volume.MetaURL)
		if err != nil {
			return fmt.Errorf("initialize volume %q: %w", name, err)
		}
		sources[name] = s
	}

	listener, err := net.Listen("tcp", cfg.Listen)
	if err != nil {
		return fmt.Errorf("listen: %w", err)
	}
	grpcServer := grpc.NewServer(
		grpc.MaxConcurrentStreams(cfg.MaxStreams),
		grpc.MaxRecvMsgSize(1024),
		grpc.MaxSendMsgSize(1<<20),
	)
	changelogv1.RegisterChangelogServer(grpcServer, server.New(sources, cursors, cfg.MaxStreams, logger))

	stop := make(chan os.Signal, 1)
	signal.Notify(stop, syscall.SIGINT, syscall.SIGTERM)
	go func() {
		<-stop
		logger.Info("graceful shutdown requested")
		grpcServer.GracefulStop()
	}()

	logger.Info("agent ready", "listen", cfg.Listen, "volumes", len(sources))
	if err := grpcServer.Serve(listener); err != nil {
		return fmt.Errorf("serve gRPC: %w", err)
	}
	return nil
}
