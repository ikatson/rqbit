OPENSSL_VERSION=3.0.3

all: sign-release sign-debug

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

target/openssl-linux/openssl-$(OPENSSL_VERSION).tar.gz:
	mkdir -p target/openssl-linux
	curl -L https://www.openssl.org/source/openssl-$(OPENSSL_VERSION).tar.gz -o $@

target/openssl-linux/x86_64/lib/libssl.a: target/openssl-linux/openssl-$(OPENSSL_VERSION).tar.gz
	export OPENSSL_ROOT=$(PWD)/target/openssl-linux/x86_64/ && \
	mkdir -p $${OPENSSL_ROOT}/src && \
	cd $${OPENSSL_ROOT}/src/ && \
	tar xf ../../openssl-$(OPENSSL_VERSION).tar.gz && \
	cd openssl-$(OPENSSL_VERSION) && \
	./Configure linux-generic64 --prefix="$${OPENSSL_ROOT}" --openssldir="$${OPENSSL_ROOT}" no-shared --cross-compile-prefix=x86_64-unknown-linux-gnu- && \
	make -j && \
	make install_sw

target/openssl-linux/aarch64/lib/libssl.a: target/openssl-linux/openssl-$(OPENSSL_VERSION).tar.gz
	export OPENSSL_ROOT=$(PWD)/target/openssl-linux/aarch64/ && \
	mkdir -p $${OPENSSL_ROOT}/src && \
	cd $${OPENSSL_ROOT}/src/ && \
	tar xf ../../openssl-$(OPENSSL_VERSION).tar.gz && \
	cd openssl-$(OPENSSL_VERSION) && \
	./Configure linux-aarch64 --prefix="$${OPENSSL_ROOT}" --openssldir="$${OPENSSL_ROOT}" no-shared --cross-compile-prefix=aarch64-unknown-linux-gnu- && \
	make -j && \
	make install_sw

target/openssl-linux/armv6/lib/libssl.a: target/openssl-linux/openssl-$(OPENSSL_VERSION).tar.gz
	export OPENSSL_ROOT=$(PWD)/target/openssl-linux/armv6/ && \
	mkdir -p $${OPENSSL_ROOT}/src && \
	cd $${OPENSSL_ROOT}/src/ && \
	tar xf ../../openssl-$(OPENSSL_VERSION).tar.gz && \
	cd openssl-$(OPENSSL_VERSION) && \
	LDFLAGS=-latomic ./Configure linux-generic32 --prefix="$${OPENSSL_ROOT}" --openssldir="$${OPENSSL_ROOT}" no-shared --cross-compile-prefix=arm-linux-gnueabihf- && \
	make -j && \
	make install_sw

@PHONY: release-linux
release-linux: release-linux-x86_64 release-linux-aarch64

@PHONY: release-linux-x86_64
release-linux-x86_64: target/openssl-linux/x86_64/lib/libssl.a
	# prereqs:
	# brew tap messense/macos-cross-toolchains
	# brew install x86_64-unknown-linux-gnu armv7-unknown-linux-gnueabihf aarch64-unknown-linux-gnu
	# cross-compile openssl with "no-shared", see above
	CC_x86_64_unknown_linux_gnu=x86_64-unknown-linux-gnu-gcc \
	CXX_x86_64_unknown_linux_gnu=x86_64-unknown-linux-gnu-g++ \
	AR_x86_64_unknown_linux_gnu=x86_64-unknown-linux-gnu-ar \
	CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=x86_64-unknown-linux-gnu-gcc \
	OPENSSL_DIR=$(PWD)/target/openssl-linux/x86_64/ \
	cargo build --release --target=x86_64-unknown-linux-gnu

@PHONY: release-linux-aarch64
release-linux-aarch64: target/openssl-linux/aarch64/lib/libssl.a
	CC_aarch64_unknown_linux_gnu=aarch64-unknown-linux-gnu-gcc \
	CXX_aarch64_unknown_linux_gnu=aarch64-unknown-linux-gnu-g++ \
	AR_aarch64_unknown_linux_gnu=aarch64-unknown-linux-gnu-ar \
	CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-unknown-linux-gnu-gcc \
	OPENSSL_DIR=$(PWD)/target/openssl-linux/aarch64/ \
	cargo build --release --target=aarch64-unknown-linux-gnu

@PHONY: release-linux-armv6
release-linux-armv6: target/openssl-linux/armv6/lib/libssl.a
	CARGO_TARGET_ARM_UNKNOWN_LINUX_GNUEABIHF_LINKER=arm-linux-gnueabihf-gcc \
	OPENSSL_DIR=$(PWD)/target/openssl-linux/armv6/ \
	cargo build --release --target=arm-unknown-linux-gnueabihf


@PHONY: release-all
release-all: release-windows release-linux release-macos-universal
	rm -rf /tmp/rqbit-release
	mkdir -p /tmp/rqbit-release
	cp ./target/x86_64-pc-windows-gnu/release/rqbit.exe /tmp/rqbit-release
	cp ./target/x86_64-apple-darwin/release/rqbit-osx-universal /tmp/rqbit-release
	cp ./target/x86_64-unknown-linux-gnu/release/rqbit /tmp/rqbit-release/rqbit-linux-x86_64
	echo "The release was built in /tmp/rqbit-release"