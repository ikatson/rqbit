OPENSSL_VERSION=3.1.1

# I'm lazy to type "webui-build" so made it default
all: webui-build

@PHONY: webui-dev
webui-dev:
	cd crates/librqbit/webui && \
	npm run dev

@PHONY: webui-build
webui-build:
	cd crates/librqbit/webui && \
	npm run build

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

target/openssl-linux/openssl-$(OPENSSL_VERSION).tar.gz:
	mkdir -p target/openssl-linux
	curl -L https://www.openssl.org/source/openssl-$(OPENSSL_VERSION).tar.gz -o $@

target/openssl-linux/$(TARGET)/lib/libssl.a: target/openssl-linux/openssl-$(OPENSSL_VERSION).tar.gz
	export OPENSSL_ROOT=$(PWD)/target/openssl-linux/$(TARGET)/ && \
	mkdir -p $${OPENSSL_ROOT}/src && \
	cd $${OPENSSL_ROOT}/src/ && \
	tar xf ../../openssl-$(OPENSSL_VERSION).tar.gz && \
	cd openssl-$(OPENSSL_VERSION) && \
	./Configure $(OPENSSL_CONFIGURE_NAME) --prefix="$${OPENSSL_ROOT}" --openssldir="$${OPENSSL_ROOT}" no-shared \
		--cross-compile-prefix=$(CROSS_COMPILE_PREFIX)- && \
	make install_dev -j

@PHONY: release-linux-current-target
release-linux-current-target: target/openssl-linux/$(TARGET)/lib/libssl.a
	CC_$(TARGET_SNAKE_CASE)=$(CROSS_COMPILE_PREFIX)-gcc \
	CXX_$(TARGET_SNAKE_CASE)=$(CROSS_COMPILE_PREFIX)-g++ \
	AR_$(TARGET_SNAKE_CASE)=$(CROSS_COMPILE_PREFIX)-ar \
	CARGO_TARGET_$(TARGET_SNAKE_UPPER_CASE)_LINKER=$(CROSS_COMPILE_PREFIX)-gcc \
	OPENSSL_DIR=$(PWD)/target/openssl-linux/$(TARGET)/ \
	cargo build  --profile release-github --target=$(TARGET)

@PHONY: release-linux
release-linux: release-linux-x86_64 release-linux-aarch64 release-linux-armv6 release-linux-armv7

@PHONY: release-linux-x86_64
release-linux-x86_64:
	TARGET=x86_64-unknown-linux-gnu \
	TARGET_SNAKE_CASE=x86_64_unknown_linux_gnu \
	TARGET_SNAKE_UPPER_CASE=X86_64_UNKNOWN_LINUX_GNU \
	CROSS_COMPILE_PREFIX=x86_64-unknown-linux-gnu \
	OPENSSL_CONFIGURE_NAME=linux-generic64 \
	$(MAKE) release-linux-current-target

@PHONY: release-linux-aarch64
release-linux-aarch64:
	TARGET=aarch64-unknown-linux-gnu \
	TARGET_SNAKE_CASE=aarch64_unknown_linux_gnu \
	TARGET_SNAKE_UPPER_CASE=AARCH64_UNKNOWN_LINUX_GNU \
	CROSS_COMPILE_PREFIX=aarch64-unknown-linux-gnu \
	OPENSSL_CONFIGURE_NAME=linux-aarch64 \
	$(MAKE) release-linux-current-target

@PHONY: release-linux-armv6
release-linux-armv6:
	TARGET=arm-unknown-linux-gnueabihf \
	TARGET_SNAKE_CASE=arm_unknown_linux_gnueabihf \
	TARGET_SNAKE_UPPER_CASE=ARM_UNKNOWN_LINUX_GNUEABIHF \
	CROSS_COMPILE_PREFIX=arm-linux-gnueabihf \
	OPENSSL_CONFIGURE_NAME=linux-generic32 \
	LDFLAGS=-latomic \
	$(MAKE) release-linux-current-target

# armv7-unknown-linux-gnueabihf
@PHONY: release-linux-armv7
release-linux-armv7:
	TARGET=armv7-unknown-linux-gnueabihf \
	TARGET_SNAKE_CASE=armv7_unknown_linux_gnueabihf \
	TARGET_SNAKE_UPPER_CASE=ARMV7_UNKNOWN_LINUX_GNUEABIHF \
	CROSS_COMPILE_PREFIX=armv7-linux-gnueabihf \
	OPENSSL_CONFIGURE_NAME=linux-generic32 \
	$(MAKE) release-linux-current-target


@PHONY: release-all
release-all: release-windows release-linux release-macos-universal
	rm -rf /tmp/rqbit-release
	mkdir -p /tmp/rqbit-release
	cp ./target/x86_64-pc-windows-gnu/release-github/rqbit.exe /tmp/rqbit-release
	cp ./target/x86_64-apple-darwin/release-github/rqbit-osx-universal /tmp/rqbit-release
	cp ./target/x86_64-unknown-linux-gnu/release-github/rqbit /tmp/rqbit-release/rqbit-linux-x86_64
	echo "The release was built in /tmp/rqbit-release"