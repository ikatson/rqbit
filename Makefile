all:

@PHONY: webui-deps
webui-deps:
	cd desktop && npm install
	cd crates/librqbit/webui && npm install

@PHONY: webui-dev
webui-dev: webui-deps
	cd crates/librqbit/webui && \
	npm run dev

@PHONY: webui-build
webui-build: webui-deps
	cd crates/librqbit/webui && \
	npm run build

@PHONY: devserver
devserver:
	echo -n '' > /tmp/rqbit-log && CORS_ALLOW_REGEXP=".*" \
	   cargo run -- \
		--log-file /tmp/rqbit-log \
		--log-file-rust-log=debug,librqbit=trace,upnp_serve=trace \
		--http-api-listen-addr 0.0.0.0:3030 \
		--upnp-server-hostname "$(shell hostname)" \
		--upnp-server-friendly-name rqbit-dev \
		server start /tmp/scratch/

@PHONY: devserver-release
devserver-profile:
	cargo run --release -- \
	   --http-api-listen-addr 0.0.0.0:3030 \
        --upnp-server-hostname "$(shell hostname)" \
        --upnp-server-friendly-name rqbit-dev \
        server start /tmp/scratch/

@PHONY: devserver
devserver-postgres:
	echo -n '' > /tmp/rqbit-log && CORS_ALLOW_REGEXP=".*" \
	   cargo run -- \
		--log-file /tmp/rqbit-log \
		--log-file-rust-log=debug,librqbit=trace \
		server start --fastresume --persistence-config postgres:///rqbit /tmp/scratch/

@PHONY: clean
clean:
	rm -rf target

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

@PHONY: test
test:
	ulimit -n unlimited && cargo test

@PHONY: release-macos-universal
release-macos-universal:
	cargo build --target aarch64-apple-darwin --profile release-github
	cargo build --target x86_64-apple-darwin --profile release-github
	lipo \
		./target/aarch64-apple-darwin/release-github/rqbit \
		./target/x86_64-apple-darwin/release-github/rqbit \
		-create \
		-output ./target/x86_64-apple-darwin/release-github/rqbit-osx-universal

@PHONY: release-windows
release-windows:
	# prereqs:
	# brew install mingw-w64
	cargo build --target x86_64-pc-windows-gnu --profile release-github

@PHONY: release-linux-current-target
release-linux-current-target:
	CC_$(TARGET_SNAKE_CASE)=$(CROSS_COMPILE_PREFIX)-gcc \
	CXX_$(TARGET_SNAKE_CASE)=$(CROSS_COMPILE_PREFIX)-g++ \
	AR_$(TARGET_SNAKE_CASE)=$(CROSS_COMPILE_PREFIX)-ar \
	CARGO_TARGET_$(TARGET_SNAKE_UPPER_CASE)_LINKER=$(CROSS_COMPILE_PREFIX)-gcc \
	cargo build  --profile release-github --target=$(TARGET) --features=openssl-vendored

@PHONY: release-linux
release-linux: release-linux-x86_64 release-linux-aarch64 release-linux-armv6 release-linux-armv7 release-linux-armv7-musl

@PHONY: release-linux-x86_64
release-linux-x86_64:
	TARGET=x86_64-unknown-linux-musl \
	TARGET_SNAKE_CASE=x86_64_unknown_linux_musl \
	TARGET_SNAKE_UPPER_CASE=X86_64_UNKNOWN_LINUX_MUSL \
	CROSS_COMPILE_PREFIX=x86_64-unknown-linux-musl \
	$(MAKE) release-linux-current-target

@PHONY: release-linux-aarch64
release-linux-aarch64:
	TARGET=aarch64-unknown-linux-gnu \
	TARGET_SNAKE_CASE=aarch64_unknown_linux_gnu \
	TARGET_SNAKE_UPPER_CASE=AARCH64_UNKNOWN_LINUX_GNU \
	CROSS_COMPILE_PREFIX=aarch64-unknown-linux-gnu \
	$(MAKE) release-linux-current-target

@PHONY: release-linux-armv6
release-linux-armv6:
	TARGET=arm-unknown-linux-gnueabihf \
	TARGET_SNAKE_CASE=arm_unknown_linux_gnueabihf \
	TARGET_SNAKE_UPPER_CASE=ARM_UNKNOWN_LINUX_GNUEABIHF \
	CROSS_COMPILE_PREFIX=arm-linux-gnueabihf \
	LDFLAGS=-latomic \
	$(MAKE) release-linux-current-target

# armv7-unknown-linux-gnueabihf
@PHONY: release-linux-armv7
release-linux-armv7:
	TARGET=armv7-unknown-linux-gnueabihf \
	TARGET_SNAKE_CASE=armv7_unknown_linux_gnueabihf \
	TARGET_SNAKE_UPPER_CASE=ARMV7_UNKNOWN_LINUX_GNUEABIHF \
	CROSS_COMPILE_PREFIX=armv7-linux-gnueabihf \
	$(MAKE) release-linux-current-target

@PHONY: release-linux-armv7-musl
release-linux-armv7-musl:
	TARGET=armv7-unknown-linux-musleabihf \
	TARGET_SNAKE_CASE=armv7_unknown_linux_musleabihf \
	TARGET_SNAKE_UPPER_CASE=ARMV7_UNKNOWN_LINUX_MUSLEABIHF \
	CROSS_COMPILE_PREFIX=armv7-linux-musleabihf \
	$(MAKE) release-linux-current-target


@PHONY: release-all
release-all: release-windows release-linux release-macos-universal
	rm -rf /tmp/rqbit-release
	mkdir -p /tmp/rqbit-release
	cp ./target/x86_64-pc-windows-gnu/release-github/rqbit.exe /tmp/rqbit-release
	cp ./target/x86_64-apple-darwin/release-github/rqbit-osx-universal /tmp/rqbit-release
	cp ./target/x86_64-unknown-linux-gnu/release-github/rqbit /tmp/rqbit-release/rqbit-linux-x86_64
	echo "The release was built in /tmp/rqbit-release"
