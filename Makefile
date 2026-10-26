# kompressor — build, test and release.
#
# `make` on its own lists what there is. Every target here is one someone
# actually runs; anything that needs a tool the machine may not have says so
# rather than failing with a compiler error from three layers down.

BIN          := kompressor
CARGO        ?= cargo
PREFIX       ?= /usr/local
DESTDIR      ?=
RELEASE_BIN  := target/release/$(BIN)
CORPUS       ?= fixtures/corpus.jsonl.gz
BASELINE     ?= fixtures/baseline.json
STATIC_TARGET?= x86_64-unknown-linux-musl
MACOS_ARCH   ?= universal2
DOCKER_IMAGE ?= kompressor
DOCKER_TAG   ?= $(shell sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)

.DEFAULT_GOAL := help
SHELL := /bin/bash

# ---------------------------------------------------------------- everyday ---

##@ Everyday

.PHONY: help
help: ## List the targets worth knowing
	@awk 'BEGIN {FS = ":.*##"; printf "\nkompressor\n"} \
		/^##@ / { printf "\n\033[1m%s\033[0m\n", substr($$0, 5) } \
		/^[a-zA-Z0-9_-]+:.*?##/ { printf "  \033[36m%-16s\033[0m %s\n", $$1, $$2 }' $(MAKEFILE_LIST)
	@echo

.PHONY: build
build: ## Debug build of the whole workspace
	$(CARGO) build --workspace

.PHONY: release
release: ## Optimised build of the binary
	$(CARGO) build --release -p kompressor-cli

.PHONY: test
test: ## Run every test
	$(CARGO) test --workspace

.PHONY: fmt
fmt: ## Format in place
	$(CARGO) fmt --all

.PHONY: lint
lint: sync-check ## Formatting, clippy, the doc links and the org invariants, as CI runs them
	$(CARGO) fmt --all --check
	$(CARGO) clippy --workspace --all-targets -- -D warnings
	# Clippy does not see intra-doc links. A dangling one sat in the websocket
	# module header pointing at a function that no longer existed anywhere in
	# the workspace, and nothing caught it: in this codebase the comments carry
	# the reasoning and the measurements, so a link that no longer resolves is
	# a measurement about to be attached to the wrong thing.
	RUSTDOCFLAGS="-D warnings" $(CARGO) doc --workspace --no-deps --quiet

# The org invariants. Not one of them announces itself when it breaks: a crate
# missing `repository` publishes with no source link at all, and an action
# outside the org allowlist fails the whole CI run at startup — zero jobs, no
# logs, and no annotation naming the line that did it. Each has been noticed
# late at least once, including after a push. Hence a gate.
#
# Hung off `lint` so it runs first and costs a second, rather than behind a
# build. Pure grep: no toolchain, no network, works in a tarball without .git.
ORG_URL   := https://github.com/hystax/optscale-kompressor
ORG_HOLDER := Hystax
SYNC_SKIP := --exclude-dir=.git --exclude-dir=target --exclude-dir=runs --exclude=Cargo.lock
# Actions the org allowlist permits. Anything else startup-fails the run, so
# this list is the allowlist, not a style preference. Check before extending.
SYNC_ACTIONS := ^(actions/|github/codeql-action|dorny/paths-filter|actions-ecosystem/|pnpm/action-setup|the-pr-agent/pr-agent)

.PHONY: sync-check
sync-check: ## Org invariants: crates carry `repository`, CI uses only allowed actions, LICENSE holder
	@fail=0; \
	missing=$$(grep -L '^repository.workspace' crates/*/Cargo.toml 2>/dev/null || true); \
	if [ -n "$$missing" ]; then \
		fail=1; \
		echo "sync-check: crates not inheriting repository (these publish with no source link):"; \
		echo "$$missing" | sed 's/^/  /'; \
		echo "  fix: add 'repository.workspace = true' under license.workspace in each"; \
	fi; \
	bad=$$(grep -hoE 'uses: [^ ]+' .github/workflows/*.yml 2>/dev/null | sed 's/^uses: //' | sort -u | grep -vE '$(SYNC_ACTIONS)' || true); \
	if [ -n "$$bad" ]; then \
		fail=1; \
		echo "sync-check: workflow actions outside the org allowlist:"; \
		echo "$$bad" | sed 's/^/  /'; \
		echo "  these fail the run at startup: no jobs, no logs, nothing naming the cause."; \
		echo "  use a GitHub-owned action, or install the toolchain with plain rustup."; \
	fi; \
	if ! grep -qE '^Copyright \(c\) [0-9]{4} $(ORG_HOLDER)$$' LICENSE 2>/dev/null; then \
		fail=1; \
		echo "sync-check: LICENSE copyright holder is not $(ORG_HOLDER):"; \
		grep -n 'Copyright' LICENSE 2>/dev/null | sed 's/^/  /' || echo "  (no Copyright line at all)"; \
		echo "  an incoming branch can carry a different holder, and it has"; \
		echo "  silently shipped here before. Restore $(ORG_HOLDER)."; \
	fi; \
	if [ $$fail -ne 0 ]; then exit 1; fi; \
	echo "sync-check: ok"

.PHONY: check
check: lint test bench bench-selftest docs-check ## Everything CI gates on (no docker; see `check-full`)

.PHONY: bench-selftest
bench-selftest: ## The benchmark reader's own checks. No build, no provider, no spend
	python3 scripts/harbor/bench.py --self-test
	python3 scripts/harbor/ab.py --self-test

.PHONY: check-full
check-full: check e2e ## `check` plus the hermetic end-to-end suite. Needs docker

.PHONY: clean
clean: ## Remove build output
	$(CARGO) clean

# ------------------------------------------------------------- correctness ---

##@ Correctness

.PHONY: test-matrix
test-matrix: ## Tests with and without the tree-sitter grammars
	$(CARGO) test --workspace
	$(CARGO) test --workspace --no-default-features

.PHONY: bench
bench: release ## Replay the committed corpus and gate on the recorded baseline
	./$(RELEASE_BIN) bench replay --corpus $(CORPUS) --compare $(BASELINE) --check-determinism

.PHONY: bench-save
bench-save: release ## Re-record the baseline. Deliberate: it moves the gate
	@echo 'This overwrites $(BASELINE), which is what "make bench" measures against.'
	@read -p "Re-record it? [y/N] " a; [ "$$a" = y ] || { echo aborted; exit 1; }
	./$(RELEASE_BIN) bench replay --corpus $(CORPUS) --save $(BASELINE)

.PHONY: bench-corpus
bench-corpus: release ## Replay another corpus: make bench-corpus CORPUS=path.jsonl.gz
	./$(RELEASE_BIN) bench replay --corpus $(CORPUS) --check-determinism

.PHONY: smoke
smoke: release ## Start every surface, exercise it end to end, stop it
	./scripts/smoke.sh

.PHONY: e2e
e2e: ## Hermetic end-to-end suite: all modes, refs, the retrieve loop, efficiency
	docker compose -f e2e/compose.yaml up --build \
	  --abort-on-container-exit --exit-code-from runner
	docker compose -f e2e/compose.yaml down -v --remove-orphans

.PHONY: e2e-down
e2e-down: ## Tear the end-to-end stack down, including its volumes
	docker compose -f e2e/compose.yaml down -v --remove-orphans

.PHONY: golden
golden: ## Re-record the compression vectors. Deliberate: it moves the snapshot
	./scripts/export-golden.sh

# --------------------------------------------------------------------- docs ---

##@ Benchmark

harbor-gates: release ## Every benchmark gate, no provider spend (needs UPSTREAM + OPENAI_API_KEY)
	SKIP_HARBOR=1 scripts/harbor/suite.sh smoke

harbor-smoke: release ## One terminal-bench task through the proxy. Costs money
	scripts/harbor/suite.sh smoke

harbor-suite: release ## The ten-task comparison set. Costs real money
	scripts/harbor/suite.sh suite

harbor-watch: ## Live view of the newest benchmark run
	scripts/harbor/watch.py

harbor-lab: release ## Offline: one codex-shaped turn through the proxy, against a recording upstream
	scripts/harbor/lab/lab.sh openai

##@ Docs

.PHONY: docs-check
docs-check: release ## Check the docs against the code they describe
	./scripts/docs/check.py

.PHONY: diagrams
diagrams: ## Re-render the charts at a uniform scale (each .drawio.png already holds its diagram)
	./scripts/docs/export-diagrams.sh

# ------------------------------------------------------------------ release ---

##@ Release

# Not the default goal: `.DEFAULT_GOAL := help` above keeps a bare `make`
# printing the target list, which is what someone arriving at this repo wants.
# The name is still `all` because that is the one every release script guesses.
.PHONY: all
all: ## Every binary and package this machine can build, into target/dist
	./scripts/build/all.sh

.PHONY: install
install: release ## Install the binary under PREFIX (default /usr/local)
	install -Dm755 $(RELEASE_BIN) $(DESTDIR)$(PREFIX)/bin/$(BIN)
	@echo "installed $(DESTDIR)$(PREFIX)/bin/$(BIN)"

.PHONY: uninstall
uninstall: ## Remove an installed binary
	rm -f $(DESTDIR)$(PREFIX)/bin/$(BIN)

.PHONY: static
static: ## Static musl binary, for the scratch image and for copying about
	./scripts/build/static.sh $(STATIC_TARGET)

.PHONY: deb
deb: release ## Debian package (needs cargo-deb)
	@command -v cargo-deb >/dev/null || { echo "cargo-deb is not installed: cargo install cargo-deb"; exit 1; }
	$(CARGO) deb -p kompressor-cli --no-build
	@ls -1 target/debian/*.deb

.PHONY: deb-static
deb-static: static ## Debian package with no libc6 dependency (musl, needs cargo-deb)
	@command -v cargo-deb >/dev/null || { echo "cargo-deb is not installed: cargo install cargo-deb"; exit 1; }
	@mkdir -p target/static
	cp -f target/$(STATIC_TARGET)/release/$(BIN) target/static/$(BIN)
	@rm -rf target/debian-static
	$(CARGO) deb -p kompressor-cli --variant musl --target $(STATIC_TARGET) --no-build \
		-o target/debian-static
	@# cargo-deb names the file after the package and version, and both are
	@# deliberately the same as the glibc package's. That leaves the filename as
	@# the only thing that can say which libc is inside, so it has to say it:
	@# downloaded side by side the two are otherwise identical until someone
	@# reads Depends, and the one that matters is the one you cannot install.
	@f=$$(ls -1 target/debian-static/*.deb); \
		mv "$$f" "$${f%_*}-static_$${f##*_}"
	@ls -1 target/debian-static/*.deb
	@dpkg-deb -f target/debian-static/*.deb Package Version Depends

.PHONY: rpm
rpm: release ## RPM package (needs cargo-generate-rpm)
	@command -v cargo-generate-rpm >/dev/null || { echo "cargo-generate-rpm is not installed: cargo install cargo-generate-rpm"; exit 1; }
	$(CARGO) generate-rpm -p crates/kompressor-cli
	@ls -1 target/generate-rpm/*.rpm

.PHONY: rpm-static
rpm-static: static ## RPM with no glibc dependency (musl, needs cargo-generate-rpm)
	@command -v cargo-generate-rpm >/dev/null || { echo "cargo-generate-rpm is not installed: cargo install cargo-generate-rpm"; exit 1; }
	@mkdir -p target/static
	cp -f target/$(STATIC_TARGET)/release/$(BIN) target/static/$(BIN)
	@rm -rf target/generate-rpm-static && mkdir -p target/generate-rpm-static
	$(CARGO) generate-rpm -p crates/kompressor-cli --variant musl \
		--auto-req disabled -o target/generate-rpm-static
	@# Same reasoning as deb-static: name and version match the glibc package
	@# deliberately, so the filename is the only place the difference can show.
	@f=$$(ls -1 target/generate-rpm-static/*.rpm); \
		mv "$$f" "$$(echo "$$f" | sed 's/\.\([^.]*\.rpm\)$$/-static.\1/')"
	@ls -1 target/generate-rpm-static/*.rpm
	@rpm -qp --requires target/generate-rpm-static/*.rpm 2>/dev/null | grep -v '^rpmlib' || true

.PHONY: macos-pkg-docker
macos-pkg-docker: ## macOS .pkg built in a container, so the host needs only Docker
	docker build -f scripts/build/Dockerfile.macos -t kompressor-macos-builder .
	docker run --rm -u "$$(id -u):$$(id -g)" -e MACOS_ARCH=$(MACOS_ARCH) \
		-v "$(CURDIR):/src" kompressor-macos-builder

.PHONY: win-builder
win-builder:
	docker build -f scripts/build/Dockerfile.windows -t kompressor-windows-builder \
		--build-arg WITH_JSIGN=$(if $(WIN_SIGN_CMD),1,0) scripts/build

# Optional Authenticode signing, see scripts/build/windows-sign.sh. Passed by
# name only, so secrets never appear on a command line; unset ones are skipped.
WIN_SIGN_ENV := -e WIN_SIGN_CMD -e WIN_SIGN_PFX -e WIN_SIGN_PASSWORD -e WIN_SIGN_TSA

.PHONY: win
win: win-builder ## Windows x86_64 .exe built in a container (signed if WIN_SIGN_* is set)
	docker run --rm -u "$$(id -u):$$(id -g)" -v "$(CURDIR):/src" $(WIN_SIGN_ENV) \
		kompressor-windows-builder ./scripts/build/windows.sh exe

.PHONY: win-installer
win-installer: win-builder ## Windows installer (setup .exe), built in the same container
	docker run --rm -u "$$(id -u):$$(id -g)" -v "$(CURDIR):/src" $(WIN_SIGN_ENV) \
		kompressor-windows-builder ./scripts/build/windows.sh installer

.PHONY: docker
docker: ## Container image from the static binary
	./scripts/build/docker.sh $(DOCKER_IMAGE) $(DOCKER_TAG) $(STATIC_TARGET)

.PHONY: audit
audit: ## Licences and advisories (needs cargo-deny)
	@command -v cargo-deny >/dev/null || { echo "cargo-deny is not installed: cargo install cargo-deny"; exit 1; }
	$(CARGO) deny check
