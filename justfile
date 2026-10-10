alias bi := build_init
alias bd := build_daemon

# PID-1 loader for aarch64 Android.
build_init:
    cross build --package esuinit --target aarch64-linux-android --release

# Magisk-derived Android daemon; ONDK must be explicitly provisioned.
build_daemon ndk abi="arm64-v8a":
    python3 scripts/magisk.py build --ndk "{{ndk}}" --abi "{{abi}}"


# Linux host standalone archive builder.
build_host:
    cargo build --locked --release --package esp-tools
clippy:
    cargo fmt
    cross clippy --target aarch64-linux-android --release
