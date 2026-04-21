.PHONY: help ami ami-init ami-validate regression-au regression-au-debug regression-pelias-au regression-roundtrip-au regression-nominatim-au pelias-refresh bench inspect-dump test

help:
	@echo "Regression / testing:"
	@echo "  test                  Run cargo test across the workspace"
	@echo "  regression-au         Run the AU regression suite (release build)"
	@echo "  regression-au-debug   Same, but use the debug profile for faster iteration"
	@echo "  bench                 Run criterion benches and refresh docs/performance/benchmarks.md"
	@echo "  inspect-dump          Dump the index to CSV under data/index/dump-csv/ for DuckDB"
	@echo "  regression-pelias-au  Run the Pelias AU corpus (partial failures expected today)"
	@echo "  regression-roundtrip-au  Ground-truth coord round-trips (reverse + housenumber)"
	@echo "  regression-nominatim-au  Hand-translated Nominatim BDD scenarios"
	@echo "  pelias-refresh        Re-fetch pelias/acceptance-tests and regenerate the AU subset"
	@echo ""
	@echo "AMI builds:"
	@echo "  ami-init              Install Packer plugins (run once)"
	@echo "  ami-validate          Validate the Packer config without building"
	@echo "  ami                   Build the AMI (requires PKRVARS=path/to/vars.hcl)"

test:
	cargo test

regression-au:
	./scripts/run-regression.sh

regression-au-debug:
	RELEASE=0 ./scripts/run-regression.sh

# Pelias acceptance-tests corpus, converted from pelias/acceptance-tests.
# Expected to have partial failure rate today — different coverage and
# ranking. Use it to track convergence vs the external gold standard.
regression-pelias-au:
	./scripts/run-regression.sh --corpus ./tests/regression/corpora/pelias-au-addresses.json

regression-roundtrip-au:
	./scripts/run-regression.sh --corpus ./tests/regression/corpora/au-roundtrip-addresses.json

regression-nominatim-au:
	./scripts/run-regression.sh --corpus ./tests/regression/corpora/nominatim-bdd-au.json

# Refresh the Pelias corpus from upstream (requires network + the
# test-data clone).
pelias-refresh:
	./scripts/fetch-test-data.sh
	./target/release/pelias-to-ours \
	    ./test-data/pelias-acceptance-tests/test_cases/australian_addresses.json \
	    --country au --name pelias-au-addresses \
	    > ./tests/regression/corpora/pelias-au-addresses.json
	@echo "refreshed pelias-au-addresses.json"

bench:
	./scripts/run-benchmarks.sh

# Dump the mmap'd index to CSV under ./data/index/dump-csv/ so DuckDB
# (or any SQL tool) can inspect streets, admin polygons, places, addr
# points, and i18n names. See docs/inspection/README.md for recipes.
inspect-dump:
	cargo build --release -p index-dumper
	./target/release/index-dumper ./data/index

ami-init:
	cd packer && packer init .

ami-validate:
	@if [ -z "$(PKRVARS)" ]; then echo "Set PKRVARS=path/to/your.pkrvars.hcl" >&2; exit 2; fi
	cd packer && packer validate -var-file=$(abspath $(PKRVARS)) geocoder.pkr.hcl

ami:
	@if [ -z "$(PKRVARS)" ]; then echo "Set PKRVARS=path/to/your.pkrvars.hcl" >&2; exit 2; fi
	cd packer && packer build -var-file=$(abspath $(PKRVARS)) geocoder.pkr.hcl
