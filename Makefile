.PHONY: build release test packaging-test lint fmt check update-deps clean install release-patch release-minor release-major

build:
	cargo build

release:
	cargo build --release

test:
	cargo test

lint:
	cargo fmt -- --check
	cargo clippy -- -D warnings

fmt:
	cargo fmt

packaging-test:
	python3 -m unittest discover -s tests/packaging -v

check: lint test packaging-test

update-deps:
	upd --apply --max-bump minor --lang rust,actions

clean:
	cargo clean

install: release
	cp target/release/unifi-cli ~/.local/bin/unifi-cli

release-patch:
	vership bump patch

release-minor:
	vership bump minor

release-major:
	vership bump major
