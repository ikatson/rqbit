all: sign-release sign-debug

sign-debug:
	codesign -f --entitlements resources/debugging.entitlements -s - target/debug/rqbit

sign-release:
	codesign -f --entitlements resources/debugging.entitlements -s - target/release/rqbit

build-release:
	cargo build --release

install: build-release
	$(MAKE) build-release
	$(MAKE) sign-release
	cp target/release/rqbit "$(HOME)/bin/"