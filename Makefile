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

@PHONY: release-macos-universal
release-macos-universal:
	cargo build --target aarch64-apple-darwin --release
	cargo build --target x86_64-apple-darwin --release
	lipo \
		./target/aarch64-apple-darwin/release/rqbit \
		./target/x86_64-apple-darwin/release/rqbit \
		-create \
		-output ./target/x86_64-apple-darwin/release/rqbit-osx-universal

@PHONY: release-windows
release-windows:
	# prereqs:
	# brew install mingw-w64
	cargo build --target x86_64-pc-windows-gnu --release

@PHONY: release-linux
release-linux:
    # prereqs:
    # brew tap messense/macos-cross-toolchains
	# brew install x86_64-unknown-linux-gnu
	# features are so that we don't need to cross-compile openssl
	export CC_x86_64_unknown_linux_gnu=x86_64-unknown-linux-gnu-gcc && \
	export CXX_x86_64_unknown_linux_gnu=x86_64-unknown-linux-gnu-g++ && \
	export AR_x86_64_unknown_linux_gnu=x86_64-unknown-linux-gnu-ar && \
	export CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=x86_64-unknown-linux-gnu-gcc && \
	cargo build --release --target=x86_64-unknown-linux-gnu --no-default-features \
		--features=sha1-rust,rust-tls


@PHONY: release-all
release-all: release-windows release-linux release-macos-universal
	rm -rf /tmp/rqbit-release
	mkdir -p /tmp/rqbit-release
	cp ./target/x86_64-pc-windows-gnu/release/rqbit.exe /tmp/rqbit-release
	cp ./target/x86_64-apple-darwin/release/rqbit-osx-universal /tmp/rqbit-release
	cp ./target/x86_64-unknown-linux-gnu/release/rqbit /tmp/rqbit-release/rqbit-linux-x86_64

	echo "The release was built in /tmp/rqbit-release"