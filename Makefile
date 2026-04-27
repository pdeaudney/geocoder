.PHONY: help ami ami-init ami-validate ami-worldwide-build regression-au regression-au-debug regression-pelias-au regression-roundtrip-au regression-nominatim-au regression-worldwide bench-accuracy bench-fixtures pelias-refresh pelias-full-refresh bench inspect-dump wof-import test builder builder-clean clean

help:
	@echo "Build:"
	@echo "  builder               Build the C++ build-index binary into ./build/"
	@echo "  builder-clean         Remove the C++ build artefacts (./build/ and builder/build/)"
	@echo "  clean                 builder-clean + cargo clean"
	@echo ""
	@echo "Regression / testing:"
	@echo "  test                  Run cargo test across the workspace"
	@echo "  regression-au         Run the AU regression suite (release build)"
	@echo "  regression-au-debug   Same, but use the debug profile for faster iteration"
	@echo "  bench                 Run criterion benches and refresh docs/performance/benchmarks.md"
	@echo "  bench-fixtures        Build / refresh the planet load-test fixtures (Geonames + Pelias)"
	@echo "  bench-accuracy        Run the bench-fixture accuracy sweep against ./data/index"
	@echo "  inspect-dump          Dump the index to CSV under data/index/dump-csv/ for DuckDB"
	@echo "  wof-import            Import WoF country polygons into WOF_INDEX_DIR (default data/index-worldwide)"
	@echo "  regression-pelias-au  Run the Pelias AU corpus (partial failures expected today)"
	@echo "  regression-roundtrip-au  Ground-truth coord round-trips (reverse + housenumber)"
	@echo "  regression-nominatim-au  Hand-translated Nominatim BDD scenarios"
	@echo "  regression-worldwide  Run full Pelias suite (AU+NZ+GB+CA+US) against data/index-worldwide"
	@echo "  pelias-refresh        Re-fetch pelias/acceptance-tests and regenerate the AU subset"
	@echo "  pelias-full-refresh   Regenerate per-country Pelias corpora (au, nz, gb, us, ca)"
	@echo ""
	@echo "AMI builds:"
	@echo "  ami-init              Install Packer plugins (run once)"
	@echo "  ami-validate          Validate the Packer config without building"
	@echo "  ami                   Build the serving AMI (requires PKRVARS=path/to/vars.hcl)"
	@echo "  ami-worldwide-build   Build the worldwide index on EC2 r8g.16xlarge (~18-24h, ~$$30-80)"

# C++ builder: configures + builds out-of-tree under ./build/. The
# canonical layout (matched by Dockerfile, run-planet-build.sh, and
# the README quickstart) puts the workspace's `build/` at the repo
# root with `builder/` as the source dir.
#
# Re-runs are incremental — cmake's regeneration is fast and `make`
# only rebuilds what changed. For a true cold rebuild, run
# `make builder-clean builder`.
NPROC := $(shell command -v nproc >/dev/null 2>&1 && nproc || sysctl -n hw.ncpu 2>/dev/null || echo 4)
builder:
	@mkdir -p build
	@cd build && cmake ../builder
	@cd build && $(MAKE) -j$(NPROC) --no-print-directory
	@echo "==> build-index ready at ./build/build-index"

# `builder/build/` shouldn't exist (the canonical pattern is
# out-of-tree under repo-root `build/`), but it appears if anyone
# runs cmake from inside builder/ — wipe both to be sure.
builder-clean:
	rm -rf build builder/build

clean: builder-clean
	cargo clean

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

# Full worldwide Pelias suite — assumes ./data/index-worldwide exists
# (combined AU+NZ+GB+CA+US build) and the per-country pelias-*-full.json
# corpora already live in tests/regression/corpora/.
regression-worldwide:
	./scripts/run-worldwide-regression.sh

# Refresh the pelias-*-full.json corpora from upstream Pelias data
# (assumes `./scripts/fetch-test-data.sh` has already cloned them).
# Iterates every alpha-2 our adapter can map from Pelias's
# country_a column, plus emits a `pelias-global-full.json` for
# cross-cutting cases that don't carry a country_a (autocomplete
# mechanics, schema, wof hierarchy — independent of geography).
# Empty outputs (countries with no cases in the Pelias corpus) are
# skipped automatically so we don't commit dozens of placeholder
# JSON files.
PELIAS_COUNTRIES := au nz gb us ca fr de nl es it br jp in mx ar at be bg ch cl cn co cr cz dk do ec eg ee fi gr hk hr hu id ie il ir is jm ke kr lk lt lu lv ma my ng no pe ph pl pt ro ru sa sg sk si se th tr tw ua uy ve vn za
pelias-full-refresh:
	python3 scripts/merge-pelias-corpus.py \
	    test-data/pelias-acceptance-tests/test_cases > /tmp/pelias-combined.json
	@for cc in $(PELIAS_COUNTRIES); do \
	    out=tests/regression/corpora/pelias-$$cc-full.json; \
	    tmp=$$(mktemp); \
	    ./target/release/pelias-to-ours /tmp/pelias-combined.json \
	        --country $$cc --name pelias-$$cc-full \
	        > $$tmp 2>/dev/null; \
	    n=$$(python3 -c "import json,sys; print(len(json.load(open('$$tmp'))['cases']))" 2>/dev/null || echo 0); \
	    if [ "$$n" -gt 0 ]; then \
	        mv $$tmp $$out; \
	        echo "  refreshed pelias-$$cc-full.json ($$n cases)"; \
	    else \
	        rm -f $$tmp $$out; \
	    fi; \
	done
	@./target/release/pelias-to-ours /tmp/pelias-combined.json \
	    --country none --name pelias-global-full \
	    > tests/regression/corpora/pelias-global-full.json 2>/dev/null; \
	n=$$(python3 -c "import json; print(len(json.load(open('tests/regression/corpora/pelias-global-full.json'))['cases']))"); \
	echo "  refreshed pelias-global-full.json ($$n cross-cutting cases)"

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

# Build / refresh the planet load-test fixtures (Geonames + Pelias).
# Run once per fixture refresh; the resulting JSONs are committed.
bench-fixtures:
	./scripts/bench/build-fixtures.sh

# Bench-fixture accuracy sweep. Defaults to ./data/index; override
# via INDEX=… for a planet build. Exit code reflects pass rate vs
# --pass-threshold (default 95 %).
INDEX ?= ./data/index
SAMPLE ?= 500
bench-accuracy:
	./scripts/run-bench-accuracy.sh --index $(INDEX) --sample $(SAMPLE)

# Import Who's on First country-level polygons as a fallback for
# Geofabrik extracts that miss their own admin_level=2 relation
# (typical of great-britain-latest and us-latest). Expects
# whosonfirst-data-admin-*.db files under test-data/ (fetched by
# scripts/fetch-test-data.sh). Writes wof_countries_*.bin into the
# given index dir — defaults to data/index-worldwide.
WOF_INDEX_DIR ?= ./data/index-worldwide
wof-import:
	cargo build --release -p wof-importer
	./target/release/wof-importer ./test-data $(WOF_INDEX_DIR)

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

# One-shot worldwide-index build on EC2. Launches r8g.16xlarge
# (Graviton 4, 512 GB RAM), downloads planet PBF + planet WoF admin,
# runs the full build pipeline, uploads the resulting index files to
# S3, terminates. Expect ~18–24 h wall-time, ~$30–80 in compute.
# Required: PKRVARS pointing at a worldwide.pkrvars.hcl with
# output_s3_prefix + build_instance_profile set.
ami-worldwide-build:
	@if [ -z "$(PKRVARS)" ]; then echo "Set PKRVARS=path/to/your.pkrvars.hcl" >&2; exit 2; fi
	cd packer && packer build -var-file=$(abspath $(PKRVARS)) build-worldwide.pkr.hcl
