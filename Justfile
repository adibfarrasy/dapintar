default:
    @just --list

b:
    @cargo build --release

t filter="":
    @cargo test --release --features integration-test {{filter}} -- --show-output
