package main

import (
	"context"
	"encoding/json"
	"flag"
	"fmt"
	"log"
	"net"
	"os"
	"os/signal"
	"syscall"

	changelogv1 "github.com/JayTsu-sh/robinhood-rs/agents/juicefs-changelog-agent/internal/gen/changelogv1"
	"google.golang.org/grpc"
	"google.golang.org/grpc/credentials/insecure"
)

func main() {
	address := flag.String("address", "10.131.9.41:9443", "Agent gRPC address")
	volume := flag.String("volume", "", "configured JuiceFS volume name")
	maxRecords := flag.Int("max-records", 0, "exit after this many records; zero means unlimited")
	ack := flag.Bool("ack", true, "acknowledge each record after writing it to stdout")
	flag.Parse()
	if *volume == "" {
		log.Fatal("-volume is required")
	}

	ctx, stop := signal.NotifyContext(context.Background(), syscall.SIGINT, syscall.SIGTERM)
	defer stop()
	dialer := &net.Dialer{}
	conn, err := grpc.NewClient(
		*address,
		grpc.WithTransportCredentials(insecure.NewCredentials()),
		grpc.WithContextDialer(func(ctx context.Context, address string) (net.Conn, error) {
			return dialer.DialContext(ctx, "tcp", address)
		}),
	)
	if err != nil {
		log.Fatalf("connect: %v", err)
	}
	defer conn.Close()
	client := changelogv1.NewChangelogClient(conn)
	stream, err := client.Watch(ctx, &changelogv1.WatchRequest{Volume: *volume})
	if err != nil {
		log.Fatalf("watch: %v", err)
	}

	encoder := json.NewEncoder(os.Stdout)
	for count := 1; ; count++ {
		record, err := stream.Recv()
		if err != nil {
			if ctx.Err() != nil {
				return
			}
			log.Fatalf("receive: %v", err)
		}
		if err := encoder.Encode(record); err != nil {
			log.Fatalf("write output: %v", err)
		}
		if *ack {
			if _, err := client.Ack(ctx, &changelogv1.AckRequest{Volume: *volume, Version: record.GetVersion()}); err != nil {
				log.Fatalf("ack version %d: %v", record.GetVersion(), err)
			}
			fmt.Fprintf(os.Stderr, "acked volume=%s version=%d\n", *volume, record.GetVersion())
		}
		if *maxRecords > 0 && count >= *maxRecords {
			return
		}
	}
}
