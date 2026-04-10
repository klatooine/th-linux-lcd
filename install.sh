#!/bin/bash
# th-linux-lcd installer
# Installs dependencies, configures USB permissions, builds, and installs the app.
# Run from the repository root.

set -euo pipefail

RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'; NC='\033[0m'
info()    { echo -e "${GREEN}[+]${NC} $*"; }
warn()    { echo -e "${YELLOW}[!]${NC} $*"; }
error()   { echo -e "${RED}[✗]${NC} $*" >&2; exit 1; }
ok()      { echo -e "${GREEN}[✓]${NC} $*"; }

# ── Detect distro ──────────────────────────────────────────────────────────────

detect_pkg_manager() {
    if command -v pacman &>/dev/null;  then echo "pacman"
    elif command -v apt-get &>/dev/null; then echo "apt"
    elif command -v dnf &>/dev/null;   then echo "dnf"
    elif command -v zypper &>/dev/null; then echo "zypper"
    else echo "unknown"
    fi
}

install_deps() {
    local pm
    pm=$(detect_pkg_manager)
    info "Detected package manager: $pm"

    case "$pm" in
        pacman)
            info "Installing dependencies (pacman)..."
            sudo pacman -S --needed --noconfirm \
                libusb python-pillow zenity \
                rust cargo
            ;;
        apt)
            info "Installing dependencies (apt)..."
            sudo apt-get update -qq
            sudo apt-get install -y \
                libusb-1.0-0 libusb-1.0-0-dev \
                python3-pil \
                zenity \
                cargo rustc
            ;;
        dnf)
            info "Installing dependencies (dnf)..."
            sudo dnf install -y \
                libusbx libusbx-devel \
                python3-pillow \
                zenity \
                cargo rust
            ;;
        zypper)
            info "Installing dependencies (zypper)..."
            sudo zypper install -y \
                libusb-1_0-0 libusb-1_0-devel \
                python3-Pillow \
                zenity \
                cargo rust
            ;;
        *)
            warn "Unknown package manager. Please install manually:"
            warn "  - libusb"
            warn "  - python3-pillow (Pillow)"
            warn "  - zenity (optional, for file picker)"
            warn "  - rust / cargo"
            warn "Continuing..."
            ;;
    esac
}

# ── Check Rust ─────────────────────────────────────────────────────────────────

check_rust() {
    if ! command -v cargo &>/dev/null; then
        warn "cargo not found. Installing via rustup..."
        curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --no-modify-path
        # shellcheck source=/dev/null
        source "$HOME/.cargo/env"
    fi
    ok "Rust $(rustc --version)"
}

# ── Build ──────────────────────────────────────────────────────────────────────

build() {
    info "Building (release)..."
    cargo build --release
    ok "Build complete: $(du -sh target/release/th-linux-lcd | cut -f1) binary"
}

# ── Install binary ─────────────────────────────────────────────────────────────

install_binary() {
    local dest="$HOME/.local/bin/th-linux-lcd"
    mkdir -p "$HOME/.local/bin"

    # Atomic replace so a running instance is not interrupted
    cp target/release/th-linux-lcd "$dest.new"
    mv "$dest.new" "$dest"
    chmod +x "$dest"
    ok "Binary installed to $dest"

    # Warn if ~/.local/bin is not in PATH
    if ! echo "$PATH" | grep -q "$HOME/.local/bin"; then
        warn "~/.local/bin is not in your PATH."
        warn "Add this to your shell profile:"
        warn "  export PATH=\"\$HOME/.local/bin:\$PATH\""
    fi
}

# ── systemd user service ───────────────────────────────────────────────────────

install_service() {
    local svc_dir="$HOME/.config/systemd/user"
    mkdir -p "$svc_dir"
    cp th-linux-lcd.service "$svc_dir/"
    systemctl --user daemon-reload
    systemctl --user enable th-linux-lcd
    ok "Systemd user service installed and enabled"
}

# ── Desktop entry ──────────────────────────────────────────────────────────────

install_desktop() {
    local apps_dir="$HOME/.local/share/applications"
    mkdir -p "$apps_dir"
    cp pkg/th-linux-lcd.desktop "$apps_dir/"
    if command -v update-desktop-database &>/dev/null; then
        update-desktop-database "$apps_dir" 2>/dev/null || true
    fi
    ok "Desktop entry installed"
}

# ── udev rules (USB permissions) ──────────────────────────────────────────────

install_udev() {
    local rules_src="pkg/99-thermaltake-lcd.rules"
    local rules_dst="/etc/udev/rules.d/99-thermaltake-lcd.rules"

    if [ ! -f "$rules_src" ]; then
        warn "udev rules file not found, skipping"
        return
    fi

    info "Installing udev rules (requires sudo)..."
    sudo cp "$rules_src" "$rules_dst"
    sudo udevadm control --reload-rules
    sudo udevadm trigger
    ok "udev rules installed: $rules_dst"

    # Add user to plugdev if needed
    if ! groups | grep -q plugdev; then
        info "Adding $USER to plugdev group..."
        sudo usermod -aG plugdev "$USER"
        warn "Group membership takes effect on next login."
        warn "You may need to log out and back in before the device is accessible."
    else
        ok "User $USER is already in plugdev"
    fi
}

# ── Main ───────────────────────────────────────────────────────────────────────

main() {
    echo ""
    echo "  th-linux-lcd installer"
    echo "  ────────────────────────────────────"
    echo ""

    # Must be run from the repo root
    if [ ! -f "Cargo.toml" ]; then
        error "Run this script from the repository root directory."
    fi

    install_deps
    check_rust
    build
    install_binary
    install_service
    install_desktop
    install_udev

    echo ""
    ok "Installation complete."
    echo ""
    echo "  Start the daemon:    systemctl --user start th-linux-lcd"
    echo "  Launch the GUI:      th-linux-lcd"
    echo "  Check daemon status: systemctl --user status th-linux-lcd"
    echo ""
    echo "  If the device is not detected, ensure the udev rule is loaded"
    echo "  and you have logged out and back in after being added to plugdev."
    echo ""
}

main "$@"
