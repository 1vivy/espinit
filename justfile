alias bi := build_init
alias bd := build_daemon

# PID-1 loader for aarch64 Android.
build_init:
    cross build --package espinit --target aarch64-linux-android --release

# espinitd daemon for aarch64 Android.
build_daemon:
    cross build --package espinitd --target aarch64-linux-android --release

clippy:
    cargo fmt
    cross clippy --target aarch64-linux-android --release
