PREFIX  ?= /usr/local
BINDIR   = $(PREFIX)/bin
BINARY   = jot-down
FEATURES ?= default

.PHONY: all build install uninstall clean

all: build

build:
	cargo build --release --features $(FEATURES)

install: build
	install -d "$(BINDIR)"
	install -m 755 target/release/$(BINARY) "$(BINDIR)/$(BINARY)"
	@echo "Installed $(BINARY) -> $(BINDIR)/$(BINARY)"

uninstall:
	rm -f "$(BINDIR)/$(BINARY)"
	@echo "Removed $(BINDIR)/$(BINARY)"

clean:
	cargo clean
