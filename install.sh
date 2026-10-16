#!/bin/bash
# th-linux-lcd installer
# Installs dependencies, configures USB permissions, builds, and installs the
# app for the current user.
#
#   bash install.sh              install or upgrade
#   bash install.sh --uninstall  remove everything except your library data

set -euo pipefail

RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'; NC='\033[0m'
info()    { echo -e "${GREEN}[+]${NC} $*"; }
warn()    { echo -e "${YELLOW}[!]${NC} $*"; }
error()   { echo -e "${RED}[✗]${NC} $*" >&2; exit 1; }
ok()      { echo -e "${GREEN}[✓]${NC} $*"; }

MIN_RUST="1.89.0"
BIN_DIR="$HOME/.local/bin"
SVC_DIR="$HOME/.config/systemd/user"
APPS_DIR="$HOME/.local/share/applications"
ICON_DIR="$HOME/.local/share/icons/hicolor/scalable/apps"
AUTOSTART_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/autostart"
RULE_NAME="70-thermaltake-lcd.rules"
OLD_RULE="/etc/udev/rules.d/99-thermaltake-lcd.rules"

have() { command -v "$1" &>/dev/null; }
version_ge() { [ "$(printf '%s\n%s\n' "$2" "$1" | sort -V | head -1)" = "$2" ]; }
user_systemd() { systemctl --user show-environment &>/dev/null; }

# ── Detect distro ──────────────────────────────────────────────────────────────

detect_distro() {
    local id="" like=""
    if [ -r /etc/os-release ]; then
        # shellcheck source=/dev/null
        id=$(. /etc/os-release && echo "${ID:-}")
        # shellcheck source=/dev/null
        like=$(. /etc/os-release && echo "${ID_LIKE:-}")
    fi
    if have rpm-ostree && [ -e /run/ostree-booted ]; then echo "ostree"; return; fi
    case " $id $like " in
        *" arch "*)                       echo "arch" ;;
        *" debian "*|*" ubuntu "*)        echo "debian" ;;
        *" fedora "*|*" rhel "*)          echo "fedora" ;;
        *" suse "*|*" opensuse "*)        echo "suse" ;;
        *)                                echo "unknown" ;;
    esac
}

install_deps() {
    local distro
    distro=$(detect_distro)
    info "Detected distro family: $distro"

    case "$distro" in
        arch)
            local pkgs=(base-devel libusb pkgconf python-pillow zenity)
            # 'rust' conflicts with 'rustup'; only add it when no toolchain exists.
            have cargo || pkgs+=(rust)
            sudo pacman -S --needed --noconfirm "${pkgs[@]}"
            ;;
        debian)
            # Distro Rust is usually too old; check_rust installs rustup if needed.
            sudo apt-get update -qq
            sudo apt-get install -y build-essential pkg-config curl libusb-1.0-0-dev python3-pil zenity
            ;;
        fedora)
            sudo dnf install -y gcc pkgconf-pkg-config curl libusb1-devel python3-pillow zenity cargo rust
            ;;
        suse)
            sudo zypper install -y gcc pkg-config curl libusb-1_0-devel python3-Pillow zenity
            ;;
        ostree)
            warn "Immutable system (rpm-ostree) detected. Layer the dependencies first, then re-run:"
            warn "  rpm-ostree install libusb1-devel python3-pillow zenity gcc && systemctl reboot"
            have pkg-config && python3 -c 'import PIL' 2>/dev/null || error "Dependencies missing."
            ;;
        *)
            warn "Unknown distro. Make sure these are installed, then re-run:"
            warn "  C compiler, pkg-config, libusb-1.0 (dev headers), python3, Pillow, zenity, curl"
            ;;
    esac
}

# ── Check Rust ─────────────────────────────────────────────────────────────────

check_rust() {
    # shellcheck source=/dev/null
    [ -f "$HOME/.cargo/env" ] && source "$HOME/.cargo/env"
    local have_ver=""
    have rustc && have_ver=$(rustc --version | awk '{print $2}')

    if [ -n "$have_ver" ] && version_ge "$have_ver" "$MIN_RUST"; then
        ok "Rust $have_ver"
        return
    fi

    if have rustup; then
        warn "Rust ${have_ver:-not found} is older than $MIN_RUST; installing the stable toolchain via rustup..."
        rustup toolchain install stable --profile minimal
    else
        warn "Rust ${have_ver:-not found} is older than $MIN_RUST."
        info "Installing a user-local toolchain with rustup (https://rustup.rs) into ~/.cargo ..."
        have curl || error "curl is required to install rustup"
        curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --no-modify-path --profile minimal
        # shellcheck source=/dev/null
        source "$HOME/.cargo/env"
    fi
    export RUSTUP_TOOLCHAIN=stable
    ok "$(rustc --version)"
}

# ── Build ──────────────────────────────────────────────────────────────────────

build() {
    info "Building (release)..."
    cargo build --release --locked --bin th-linux-lcd
    ok "Build complete: $(du -sh target/release/th-linux-lcd | cut -f1) binary"
}

# ── Install binary ─────────────────────────────────────────────────────────────

install_binary() {
    local dest="$BIN_DIR/th-linux-lcd"
    mkdir -p "$BIN_DIR"

    # Atomic replace so a running instance is not interrupted
    install -m755 target/release/th-linux-lcd "$dest.new"
    mv -f "$dest.new" "$dest"
    ok "Binary installed to $dest"

    case ":$PATH:" in
        *":$BIN_DIR:"*) ;;
        *)
            warn "~/.local/bin is not in your PATH. Add this to your shell profile:"
            warn "  export PATH=\"\$HOME/.local/bin:\$PATH\""
            ;;
    esac
}

# ── systemd user service ───────────────────────────────────────────────────────

install_service() {
    mkdir -p "$SVC_DIR"
    sed 's|^ExecStart=/usr/bin/|ExecStart=%h/.local/bin/|' th-linux-lcd.service > "$SVC_DIR/th-linux-lcd.service"
    if user_systemd; then
        systemctl --user daemon-reload
        systemctl --user enable th-linux-lcd
        # Restart so an upgrade picks up the new binary (waits for any flash write to finish).
        systemctl --user restart th-linux-lcd
        ok "Daemon service installed, enabled and started"
    else
        warn "No systemd user session (SSH/TTY?). After logging in graphically run:"
        warn "  systemctl --user enable --now th-linux-lcd"
    fi
}

# ── Desktop entry + icon ───────────────────────────────────────────────────────

install_desktop() {
    mkdir -p "$APPS_DIR" "$ICON_DIR"
    install -m644 pkg/th-linux-lcd.svg "$ICON_DIR/th-linux-lcd.svg"
    local exec_path="$BIN_DIR/th-linux-lcd"
    exec_path=${exec_path//\\/\\\\}
    exec_path=${exec_path//&/\\&}
    exec_path=${exec_path//|/\\|}
    sed "s|^Exec=th-linux-lcd|Exec=\"$exec_path\"|" pkg/th-linux-lcd.desktop > "$APPS_DIR/th-linux-lcd.desktop"
    if have update-desktop-database; then
        update-desktop-database "$APPS_DIR" 2>/dev/null || true
    fi
    ok "Desktop entry installed"
}

# ── Tray icon (XDG autostart) ──────────────────────────────────────────────────

install_tray() {
    mkdir -p "$AUTOSTART_DIR"
    local exec_path="$BIN_DIR/th-linux-lcd"
    exec_path=${exec_path//\\/\\\\}
    exec_path=${exec_path//&/\\&}
    exec_path=${exec_path//|/\\|}
    sed "s|^Exec=th-linux-lcd|Exec=\"$exec_path\"|" pkg/th-linux-lcd-tray.desktop > "$AUTOSTART_DIR/th-linux-lcd-tray.desktop"
    ok "Tray icon will start at login (remove $AUTOSTART_DIR/th-linux-lcd-tray.desktop to disable)"

    # (Re)start it now so an upgrade picks up the new binary.
    restart_tray
}

# Stop only our tray process (never the daemon or GUI).
stop_tray() {
    local pid
    for pid in $(pgrep -u "$(id -u)" -x th-linux-lcd || true); do
        if tr '\0' ' ' < "/proc/$pid/cmdline" 2>/dev/null | grep -q -- ' --tray'; then
            kill "$pid" 2>/dev/null || true
        fi
    done
}

restart_tray() {
    stop_tray
    if [ -n "${WAYLAND_DISPLAY:-}${DISPLAY:-}" ] && [ -n "${DBUS_SESSION_BUS_ADDRESS:-}" ]; then
        sleep 0.5
        setsid -f "$BIN_DIR/th-linux-lcd" --tray </dev/null >/dev/null 2>&1 || true
        ok "Tray icon started"
    fi
}

# ── udev rule (USB permissions) ────────────────────────────────────────────────

install_udev() {
    info "Installing udev rule (requires sudo)..."
    sudo install -Dm644 "pkg/$RULE_NAME" "/etc/udev/rules.d/$RULE_NAME"
    sudo rm -f "$OLD_RULE"
    sudo udevadm control --reload-rules
    sudo udevadm trigger --action=change --subsystem-match=usb --attr-match=idVendor=264a
    sudo udevadm settle || true
    ok "udev rule installed (access is granted to the logged-in user; no group or re-login needed)"
}

# ── Uninstall ──────────────────────────────────────────────────────────────────

uninstall() {
    info "Uninstalling th-linux-lcd..."
    if user_systemd; then
        systemctl --user disable --now th-linux-lcd 2>/dev/null || true
    fi
    stop_tray
    rm -f "$BIN_DIR/th-linux-lcd" "$SVC_DIR/th-linux-lcd.service" \
          "$APPS_DIR/th-linux-lcd.desktop" "$ICON_DIR/th-linux-lcd.svg" \
          "$AUTOSTART_DIR/th-linux-lcd-tray.desktop"
    user_systemd && systemctl --user daemon-reload || true
    sudo rm -f "/etc/udev/rules.d/$RULE_NAME" "$OLD_RULE"
    sudo udevadm control --reload-rules
    ok "Uninstalled. Your library is kept in ~/.local/share/th-linux-lcd (delete it manually if you want)."
}

# ── Main ───────────────────────────────────────────────────────────────────────

main() {
    [ "$(id -u)" -eq 0 ] && error "Do not run as root. Run as your normal user; sudo is used where needed."
    cd "$(dirname "$(readlink -f "$0")")"
    [ -f Cargo.toml ] && [ -f th-linux-lcd.service ] || error "Run this script from the th-linux-lcd repository."

    case "${1:-}" in
        --uninstall) uninstall; return ;;
        "") ;;
        *) error "Unknown option: $1 (use --uninstall to remove)" ;;
    esac

    echo ""
    echo "  th-linux-lcd installer"
    echo "  ────────────────────────────────────"
    echo "  This asks for your sudo password to install packages and a udev rule."
    echo ""

    install_deps
    check_rust
    build
    install_binary
    install_desktop
    install_udev
    install_service
    install_tray

    echo ""
    ok "Installation complete."
    echo ""
    echo "  Launch the GUI:      th-linux-lcd   (or click the tray icon / find \"TH Linux LCD\" in your app menu)"
    echo "  Daemon status:       systemctl --user status th-linux-lcd"
    echo "  Daemon logs:         journalctl --user -u th-linux-lcd -f"
    echo "  Uninstall:           bash install.sh --uninstall"
    echo ""
    echo "  If the GUI shows NO DEVICE, unplug and re-plug the cooler's USB cable."
    echo ""
}

main "$@"
