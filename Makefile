all: sign-release sign-debug

@PHONY: sign-debug
sign-debug:
	codesign -f --entitlements resources/debugging.entitlements -s - target/debug/rqbit

@PHONY: sign-release
sign-release:
	codesign -f --entitlements resources/debugging.entitlements -s - target/release/rqbit

@PHONY: build-release
build-release:
	cargo build --release

@PHONY: install
install: build-release
	$(MAKE) build-release
	$(MAKE) sign-release
	install target/release/rqbit "$(HOME)/bin/"