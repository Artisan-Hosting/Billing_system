CONFIG    ?= examples/billing.json
ENV_FILE  ?= examples/billing.env

.PHONY: build release run migrate test fmt fmt-check clippy check clean

build:
	cargo build

release:
	cargo build --release

## Run the gRPC service + usage-cost HTTP server against the example config.
run: build
	cargo run -- --config $(CONFIG) --env-file $(ENV_FILE)

## Apply pending database migrations and exit.
migrate: build
	cargo run -- --config $(CONFIG) --env-file $(ENV_FILE) migrate

test:
	cargo test

fmt:
	cargo fmt

fmt-check:
	cargo fmt -- --check

clippy:
	cargo clippy --all-targets -- -D warnings

check: fmt-check clippy test

clean:
	cargo clean
