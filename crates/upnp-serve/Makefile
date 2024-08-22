all: release-linux-armv7-musl

@PHONY: release-linux-current-target
release-linux-current-target:
	CC_$(TARGET_SNAKE_CASE)=$(CROSS_COMPILE_PREFIX)-gcc \
	CXX_$(TARGET_SNAKE_CASE)=$(CROSS_COMPILE_PREFIX)-g++ \
	AR_$(TARGET_SNAKE_CASE)=$(CROSS_COMPILE_PREFIX)-ar \
	CARGO_TARGET_$(TARGET_SNAKE_UPPER_CASE)_LINKER=$(CROSS_COMPILE_PREFIX)-gcc \
	cargo build --target=$(TARGET) --features=openssl/vendored

@PHONY: release-linux-armv7-musl
release-linux-armv7-musl:
	TARGET=armv7-unknown-linux-musleabihf \
	TARGET_SNAKE_CASE=armv7_unknown_linux_musleabihf \
	TARGET_SNAKE_UPPER_CASE=ARMV7_UNKNOWN_LINUX_MUSLEABIHF \
	CROSS_COMPILE_PREFIX=armv7-linux-musleabihf \
	$(MAKE) release-linux-current-target
