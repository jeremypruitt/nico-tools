.PHONY: smoke run-doctor run-correlate

smoke: ## Run smoke tests against a live cluster (requires .env.local)
	./scripts/smoke.sh

run-doctor: ## Quick run of nico-doctor (requires .env.local)
	source .env.local && cargo run -p nico-doctor -- $(ARGS)

run-correlate: ## Quick run of nico-correlate (requires .env.local)
	source .env.local && cargo run -p nico-correlate -- $(ARGS)
