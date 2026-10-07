#!/bin/sh
set -eu

GKI_ROOT=$(pwd)

display_usage() {
    echo "Usage: $0 [--cleanup | <commit-or-tag>]"
    echo "  --cleanup:              Cleans up previous modifications made by the script."
    echo "  <commit-or-tag>:        Sets up or updates esu to specified tag or commit."
    echo "  -h, --help:             Displays this usage information."
    echo "  (no args):              Sets up or updates the esu environment to the latest tagged version."
}

initialize_variables() {
    if test -d "$GKI_ROOT/common/drivers"; then
         DRIVER_DIR="$GKI_ROOT/common/drivers"
    elif test -d "$GKI_ROOT/drivers"; then
         DRIVER_DIR="$GKI_ROOT/drivers"
    else
         echo '[ERROR] "drivers/" directory not found.'
         exit 127
    fi

    DRIVER_MAKEFILE=$DRIVER_DIR/Makefile
    DRIVER_KCONFIG=$DRIVER_DIR/Kconfig
}

# Reverts modifications made by this script
perform_cleanup() {
    echo "[+] Cleaning up..."
    [ -L "$DRIVER_DIR/esu" ] && rm "$DRIVER_DIR/esu" && echo "[-] Symlink removed."
    grep -q "esu" "$DRIVER_MAKEFILE" && sed -i '/esu/d' "$DRIVER_MAKEFILE" && echo "[-] Makefile reverted."
    grep -q "drivers/esu/Kconfig" "$DRIVER_KCONFIG" && sed -i '/drivers\/esu\/Kconfig/d' "$DRIVER_KCONFIG" && echo "[-] Kconfig reverted."
    if [ -d "$GKI_ROOT/esu" ]; then
        rm -rf "$GKI_ROOT/esu" && echo "[-] esu directory deleted."
    fi
}

# Sets up or updates the esu environment
setup_esu() {
    echo "[+] Setting up esu..."
    test -d "$GKI_ROOT/esu" || git clone https://github.com/1vivy/kernelesp "$GKI_ROOT/esu" && echo "[+] Repository cloned."
    cd "$GKI_ROOT/esu"
    git stash && echo "[-] Stashed current changes."
    if [ "$(git status | grep -Po 'v\d+(\.\d+)*' | head -n1)" ]; then
        git checkout main && echo "[-] Switched to main branch."
    fi
    git pull && echo "[+] Repository updated."
    if [ -z "${1-}" ]; then
        git checkout "$(git describe --abbrev=0 --tags)" && echo "[-] Checked out latest tag."
    else
        git checkout "$1" && echo "[-] Checked out $1." || echo "[-] Checkout default branch"
    fi
    cd "$DRIVER_DIR"
    ln -sf "$(realpath --relative-to="$DRIVER_DIR" "$GKI_ROOT/esu/kernel")" "esu" && echo "[+] Symlink created."

    # Add entries in Makefile and Kconfig if not already existing
    grep -q "esu" "$DRIVER_MAKEFILE" || printf "\nobj-\$(CONFIG_KERNELESP) += esu/\n" >> "$DRIVER_MAKEFILE" && echo "[+] Modified Makefile."
    grep -q "source \"drivers/esu/Kconfig\"" "$DRIVER_KCONFIG" || sed -i "/endmenu/i\source \"drivers/esu/Kconfig\"" "$DRIVER_KCONFIG" && echo "[+] Modified Kconfig."
    echo '[+] Done.'
}

# Process command-line arguments
if [ "$#" -eq 0 ]; then
    initialize_variables
    setup_esu
elif [ "$1" = "-h" ] || [ "$1" = "--help" ]; then
    display_usage
elif [ "$1" = "--cleanup" ]; then
    initialize_variables
    perform_cleanup
else
    initialize_variables
    setup_esu "$@"
fi
