# Top-level wrapper for the Rust core + macOS shell build.
# See docs/v0.1-design.md and CLAUDE.md for the architecture.

CORE_DIR := core
MACOS_DIR := shells/macos
XCODEPROJ := $(MACOS_DIR)/SpeakerAIConnector.xcodeproj
RELEASE_DIR := $(MACOS_DIR)/build/Build/Products/Release

# Version label used for the release zip filename. Defaults to `git describe`
# so a tagged commit produces `SpeakerAIConnector-v0.1.0.zip`, matching the
# artifact produced by .github/workflows/release.yml.
TAG ?= $(shell git describe --tags --always 2>/dev/null || echo dev)

.PHONY: all build core generate app test zip release clean help

all: build

# Build the Rust core and regenerate the macOS Xcode project.
build: core generate

core:
	cd $(CORE_DIR) && cargo build

generate: $(XCODEPROJ)

$(XCODEPROJ): $(MACOS_DIR)/project.yml
	cd $(MACOS_DIR) && xcodegen generate

# Full app build via xcodebuild (also runs the cargo build preBuildScript).
app: generate
	xcodebuild -project $(XCODEPROJ) -scheme SpeakerAIConnector -configuration Debug build

test:
	cd $(CORE_DIR) && cargo test

# Local equivalent of .github/workflows/release.yml: Release-config xcodebuild,
# ad-hoc codesign, ditto zip, and a SHA-256 sidecar — same artifact shape as
# the GitHub Release, just not produced from a tagged CI run. Override TAG
# (e.g. `make zip TAG=v0.1.0-local`) to control the zip filename.
zip release: generate
	cd $(MACOS_DIR) && xcodebuild \
		-project SpeakerAIConnector.xcodeproj \
		-scheme SpeakerAIConnector \
		-configuration Release \
		-derivedDataPath build \
		CODE_SIGN_IDENTITY="-" \
		CODE_SIGNING_REQUIRED=NO \
		CODE_SIGNING_ALLOWED=NO \
		build
	cd $(RELEASE_DIR) && codesign --force --deep --sign - SpeakerAIConnector.app
	cd $(RELEASE_DIR) && codesign --verify --deep --strict --verbose=2 SpeakerAIConnector.app
	cd $(RELEASE_DIR) && ditto -c -k --keepParent SpeakerAIConnector.app "SpeakerAIConnector-$(TAG).zip"
	cd $(RELEASE_DIR) && shasum -a 256 "SpeakerAIConnector-$(TAG).zip" > "SpeakerAIConnector-$(TAG).zip.sha256"
	@echo ""
	@echo "Built $(RELEASE_DIR)/SpeakerAIConnector-$(TAG).zip"
	@cat $(RELEASE_DIR)/SpeakerAIConnector-$(TAG).zip.sha256

clean:
	cd $(CORE_DIR) && cargo clean
	rm -rf $(XCODEPROJ)
	rm -rf $(MACOS_DIR)/build

help:
	@echo "Targets:"
	@echo "  build      cargo build + xcodegen generate (default)"
	@echo "  core       cargo build only"
	@echo "  generate   xcodegen generate only"
	@echo "  app        full xcodebuild of the macOS shell (Debug)"
	@echo "  zip        Release xcodebuild + ad-hoc sign + ditto zip (alias: release)"
	@echo "  test       cargo test"
	@echo "  clean      cargo clean + remove the generated .xcodeproj and build/"
