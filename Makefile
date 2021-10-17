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

@PHONY: release-all
release-all:
	cargo build --target aarch64-apple-darwin --release
	cargo build --target x86_64-apple-darwin --release
	# brew install mingw-w64 for this to work
	cargo build --target x86_64-pc-windows-gnu --release

	rm -rf /tmp/rqbit-release
	mkdir -p /tmp/rqbit-release
	lipo ./target/aarch64-apple-darwin/release/rqbit ./target/x86_64-apple-darwin/release/rqbit -create -output /tmp/rqbit-release/rqbit-osx-universal
	cp ./target/x86_64-pc-windows-gnu/release/rqbit.exe /tmp/rqbit-release

	echo "The release was built in /tmp/rqbit-release"