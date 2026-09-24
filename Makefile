# subx — repo-root Makefile, portable on cmd and sh.
# Usage: make <target>  |  make run ARGS="probe --help"

CARGO := cargo

.PHONY: help build build-release test test-e2e lint fmt fmt-check check ci install clean run

help:
	@echo "Targets:"
	@echo "  make build          - debug build"
	@echo "  make build-release  - optimized release build"
	@echo "  make test           - unit tests (ffmpeg not required)"
	@echo "  make test-e2e       - end-to-end tests (needs ffmpeg/ffprobe)"
	@echo "  make lint           - clippy, warnings denied"
	@echo "  make fmt            - format code"
	@echo "  make fmt-check      - fail if unformatted"
	@echo "  make check | ci     - fmt-check + lint + test"
	@echo "  make install        - cargo install the binary"
	@echo "  make clean          - drop build artifacts"
	@echo "  make run ARGS=...   - run, e.g. ARGS=\"extract --help\""

build:
	$(CARGO) build

build-release:
	$(CARGO) build --release

test:
	$(CARGO) test

test-e2e:
	$(CARGO) test -- --ignored

lint:
	$(CARGO) clippy -- -D warnings

fmt:
	$(CARGO) fmt

fmt-check:
	$(CARGO) fmt -- --check

check: fmt-check lint test

ci: check

install:
	$(CARGO) install --path .

clean:
	$(CARGO) clean

run:
ifndef ARGS
	@echo "Usage: make run ARGS=\"<command> [options]\""
	@echo "Example: make run ARGS=\"probe --help\""
else
	$(CARGO) run -- $(ARGS)
endif
