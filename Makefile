.PHONY: build check test fmt install uninstall

build:
	cargo build

check:
	cargo fmt --check
	cargo clippy --all-targets -- -D warnings

test:
	cargo test --all

fmt:
	cargo fmt

install:
	sudo bash scripts/install.sh

uninstall:
	sudo bash scripts/install.sh --uninstall
