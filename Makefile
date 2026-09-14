# retrofeel — developer convenience targets. Run `make` (or `make help`) for a list.
#
# The headline is `make demo`: it checks disk pressure, discovers Cargo's
# active target directory, refuses to clean while a compiler is active, then
# cleans only this package before launching the real app flow. Mock cores
# remain available through explicit QA targets.
#
# `make run` launches the app with no core (opens straight on the Library) and
# skips the clean, for fast iteration.

CARGO ?= cargo
APP := retrofeel

.PHONY: demo run build mock-core clean check test help
.DEFAULT_GOAL := help

demo: ## Clean rebuild, then launch the real app flow
	CARGO="$(CARGO)" scripts/demo.sh

run: ## Launch the app with no core (opens the Library); no clean, for fast iteration
	$(CARGO) run -p $(APP)

build: ## Build the app + the mock core
	$(CARGO) build -p mock-core
	$(CARGO) build -p $(APP)

mock-core: ## Build just the mock core cdylib test fixture
	$(CARGO) build -p mock-core

clean: ## Remove all build artifacts (forces a from-scratch rebuild)
	$(CARGO) clean

check: ## Run fmt --check + clippy -D warnings, as CI does
	$(CARGO) fmt --all -- --check
	RUSTFLAGS="-D warnings" $(CARGO) clippy --workspace --all-targets

test: ## Run the workspace test suite
	$(CARGO) test --workspace

help: ## List available targets
	@grep -E '^[a-zA-Z_-]+:.*?## ' $(MAKEFILE_LIST) \
		| awk 'BEGIN {FS = ":.*?## "}; {printf "  make %-10s %s\n", $$1, $$2}'
