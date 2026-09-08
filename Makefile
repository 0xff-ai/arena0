.PHONY: all

all:
	cd programs && cargo run --manifest-path ../Cargo.toml -p cargo-arena0 -- build
	cargo install --locked --path crates/arena0-cli
	cargo install --locked --path crates/arena0d
