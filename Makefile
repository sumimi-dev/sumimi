# Sumimi — build helpers.
#
# The build compiles DuckDB into the executable, so the release is a single
# self-contained binary: one file to copy, nothing else to install.

CARGO ?= cargo
CARGO_HTTP_TIMEOUT ?= 600
TARGET ?= target/release
# The version of the release: the crate version, not DuckDB's.
VERSION ?= 0.4.1
.PHONY: all build release test lint fmt run clean package

all: build

## Build (debug).
build:
	$(CARGO) build

## Build (release): static DuckDB, self-contained binaries.
release:
	CARGO_HTTP_TIMEOUT=$(CARGO_HTTP_TIMEOUT) $(CARGO) build --release

## Run the tests.
test:
	$(CARGO) test --workspace

## Run clippy with warnings as errors.
lint:
	$(CARGO) clippy --workspace --all-targets -- -D warnings

## Format the code.
fmt:
	$(CARGO) fmt --all

## Run the server locally.
run:
	$(CARGO) run -p sumimi-service -- serve --db ./dev.duckdb --port 3000

## Build the release artefact.
##
##   sumimi-v<ver>-<os>-<arch>.tar.gz
##       the `sumimi` binary (server + client) + js/ + docs/
##
## One binary: the server and the client share the schema, the metric definitions and
## the query code, so there is only one copy of DuckDB in it.
package: release
	@set -e; \
	rm -rf dist; mkdir -p dist/bundle; \
	cp $(TARGET)/sumimi dist/bundle/; \
	ln -sf sumimi dist/bundle/sumimi-server; \
	ln -sf sumimi dist/bundle/sumimi-client; \
	cp -r js dist/bundle/js; cp -r service/docs dist/bundle/docs; \
	tar czf sumimi-v$(VERSION)-$$(uname -s)-$$(uname -m).tar.gz -C dist/bundle .; \
	echo "wrote sumimi-v$(VERSION)-$$(uname -s)-$$(uname -m).tar.gz"

## Remove the build artefacts.
clean:
	$(CARGO) clean
