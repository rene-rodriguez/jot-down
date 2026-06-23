PREFIX  ?= /usr/local
BINDIR   = $(PREFIX)/bin
BINARY   = jot-down
# Default build is batteries-included (ai + syntax-highlight + images). For a
# minimal binary: make build CARGO_FLAGS="--no-default-features --features ai"
CARGO_FLAGS ?=

.PHONY: all build install uninstall clean

all: build

build:
	cargo build --release $(CARGO_FLAGS)

install: build
	install -d "$(BINDIR)"
	install -m 755 target/release/$(BINARY) "$(BINDIR)/$(BINARY)"
	@echo "Installed $(BINARY) -> $(BINDIR)/$(BINARY)"

uninstall:
	rm -f "$(BINDIR)/$(BINARY)"
	@echo "Removed $(BINDIR)/$(BINARY)"

clean:
	cargo clean
