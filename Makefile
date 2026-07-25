# stt-linux
#
#   make                      build stt and sttd in release mode
#   make install              install into ~/.local/bin (no root needed)
#   sudo make install PREFIX=/usr/local
#   make uninstall
#
# FEATURES selects an ONNX Runtime execution provider, e.g.
#
#   make install FEATURES=openvino     # Intel iGPU
#   make install FEATURES=webgpu       # portable GPU
#   make install FEATURES=cuda
#
# DESTDIR is honoured for packaging:  make install DESTDIR=/tmp/pkg PREFIX=/usr

CARGO    ?= cargo
PREFIX   ?= $(HOME)/.local
BINDIR   ?= $(PREFIX)/bin
DESTDIR  ?=
FEATURES ?=

feat := $(if $(FEATURES),--features $(FEATURES),)

.PHONY: all build install uninstall test lint fmt clean help

all: build

build:
	$(CARGO) build --release --locked -p sttd $(feat)
	$(CARGO) build --release --locked -p stt $(feat)

install: build
	install -Dm755 target/release/stt  $(DESTDIR)$(BINDIR)/stt
	install -Dm755 target/release/sttd $(DESTDIR)$(BINDIR)/sttd
	@echo
	@echo "Installed into $(DESTDIR)$(BINDIR)."
	@echo "Next: stt model download   (~610 MB, once)"
	@echo "Then: stt doctor"

uninstall:
	rm -f $(DESTDIR)$(BINDIR)/stt $(DESTDIR)$(BINDIR)/sttd

test:
	$(CARGO) test --workspace

lint:
	$(CARGO) clippy --all-targets

fmt:
	$(CARGO) fmt --all --check

clean:
	$(CARGO) clean

help:
	@echo "targets: build install uninstall test lint fmt clean"
	@echo "vars:    PREFIX=$(PREFIX) BINDIR=$(BINDIR) FEATURES=$(FEATURES)"
