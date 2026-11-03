# Requires GNU Make (`winget install ezwinports.make`).
#
#   make            debug build
#   make run        build and launch
#   make release    optimized build
#   make test       unit tests
#   make fmt        format the code
#   make lint       clippy, warnings are errors
#   make check      fmt-check + lint + test
#   make clean      remove target/

.DEFAULT_GOAL := build
.PHONY: build run release test fmt fmt-check lint check clean

build:
	cargo build

run:
	cargo run

release:
	cargo build --release

test:
	cargo test

fmt:
	cargo fmt

fmt-check:
	cargo fmt --check

lint:
	cargo clippy --all-targets -- -D warnings

check: fmt-check lint test

clean:
	cargo clean
