.PHONY: build test proto

build:
	go build -tags fdb -trimpath -o bin/juicefs-changelog-agent ./cmd/juicefs-changelog-agent
	go build -trimpath -o bin/changelog-receiver ./cmd/changelog-receiver

test:
	go test ./...

# Requires protoc, protoc-gen-go, and protoc-gen-go-grpc.
proto:
	protoc -I . \
		--go_out=. --go_opt=module=github.com/JayTsu-sh/robinhood-rs/agents/juicefs-changelog-agent \
		--go-grpc_out=. --go-grpc_opt=module=github.com/JayTsu-sh/robinhood-rs/agents/juicefs-changelog-agent \
		api/v1/changelog.proto
