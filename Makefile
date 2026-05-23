# Top-level wrapper for the Rust core + macOS shell build.
# See docs/v0.1-design.md and CLAUDE.md for the architecture.

CORE_DIR := core
MACOS_DIR := shells/macos
XCODEPROJ := $(MACOS_DIR)/SpeakerAIConnector.xcodeproj

.PHONY: all build core generate app test clean help

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

clean:
	cd $(CORE_DIR) && cargo clean
	rm -rf $(XCODEPROJ)

help:
	@echo "Targets:"
	@echo "  build      cargo build + xcodegen generate (default)"
	@echo "  core       cargo build only"
	@echo "  generate   xcodegen generate only"
	@echo "  app        full xcodebuild of the macOS shell"
	@echo "  test       cargo test"
	@echo "  clean      cargo clean + remove the generated .xcodeproj"
