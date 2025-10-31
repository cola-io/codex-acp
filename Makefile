SHELL := /bin/bash
.PHONY: build release run run-mock fmt clippy check test lint all clean

CARGO := cargo

build:
	$(CARGO) build

release:
	$(CARGO) build --release

run:
	RUST_LOG?=info
	RUST_LOG=$(RUST_LOG) $(CARGO) run --quiet

fmt:
	$(CARGO) fmt --all

clippy:
	$(CARGO) clippy -- -D warnings

check:
	$(CARGO) check

test:
	$(CARGO) test

lint: fmt clippy

clean:
	$(CARGO) clean

all: fmt clippy build
