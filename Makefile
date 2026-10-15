.DEFAULT_GOAL := help
.PHONY: help install run test lint fmt

help: ## Show this help
	@awk 'BEGIN {FS = ":.*## "} /^[a-z-]+:.*## / {printf "  %-8s %s\n", $$1, $$2}' $(MAKEFILE_LIST)

install: ## Install mijote into ~/.cargo/bin
	cargo install --path .

run: ## Run mijote (pass args with ARGS="agents")
	cargo run -- $(ARGS)

test: ## Run the test suite
	cargo test

lint: ## Clippy, warnings as errors
	cargo clippy --all-targets -- -D warnings

fmt: ## Format the code
	cargo fmt --all
