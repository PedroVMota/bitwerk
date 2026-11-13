# bitwerk — common tasks. Run `make help` for the list.
CARGO ?= cargo

.PHONY: help build release run test lint fmt fmt-check audit fuzz fuzz-check bench sweep icon bundle check-all clean

help:
	@echo "build      - debug build of the whole workspace"
	@echo "release    - optimized build (LTO, overflow checks on)"
	@echo "run        - run the app (debug)"
	@echo "test       - run all tests"
	@echo "lint       - clippy (pedantic) with warnings as errors"
	@echo "fmt        - format the code"
	@echo "audit      - cargo deny check (advisories, licenses, bans, sources)"
	@echo "fuzz       - fuzz the project loader and the .map parser (nightly + cargo-fuzz)"
	@echo "bench      - headless 100k-object GPU frame-cost measurement"
	@echo "sweep      - scripted 20 s GUI measurement on the 100k stress test (opens a window)"
	@echo "icon       - render the app icon from code into assets/ (PNG + .icns via iconutil)"
	@echo "bundle     - release build packaged as target/bitwerk.app (Info.plist + icon)"
	@echo "check-all  - fmt-check + lint + test + audit"

build:
	$(CARGO) build --workspace

release:
	$(CARGO) build --workspace --release

run:
	$(CARGO) run -p bw-app

test:
	$(CARGO) test --workspace

lint:
	$(CARGO) clippy --workspace --all-targets -- -D warnings

fmt:
	$(CARGO) fmt --all

fmt-check:
	$(CARGO) fmt --all -- --check

audit:
	$(CARGO) deny check

fuzz:
	$(CARGO) +nightly fuzz run load_project fuzz/seeds/load_project -- -max_total_time=300
	$(CARGO) +nightly fuzz run load_memory_map fuzz/seeds/load_memory_map -- -max_total_time=300

# Compiles the fuzz crate on stable without running it.
fuzz-check:
	$(CARGO) check --manifest-path fuzz/Cargo.toml

bench:
	$(CARGO) test --release -p bw-render --test gpu_smoke -- --ignored --nocapture

sweep:
	$(CARGO) run --release -p bw-app -- --stress --sweep

# The icon is drawn by code (bw_render::symbols::raster::app_icon): PNGs for
# every .iconset size, then iconutil (part of macOS) builds the .icns. The
# results are committed in assets/, so normal builds do not need this.
ICONSET := target/AppIcon.iconset
icon:
	$(CARGO) run --release -p bw-app --bin bw-icon -- $(ICONSET) assets/AppIcon-256.png
	iconutil -c icns $(ICONSET) -o assets/AppIcon.icns

APP := target/bitwerk.app
bundle: release
	rm -rf $(APP)
	mkdir -p $(APP)/Contents/MacOS $(APP)/Contents/Resources
	cp target/release/bitwerk $(APP)/Contents/MacOS/bitwerk
	cp assets/Info.plist $(APP)/Contents/Info.plist
	cp assets/AppIcon.icns $(APP)/Contents/Resources/AppIcon.icns
	@echo "built $(APP) (open it with: open $(APP))"

check-all: fmt-check lint test audit

clean:
	$(CARGO) clean
