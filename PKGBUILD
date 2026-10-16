# Maintainer: klatooine
pkgname=th-linux-lcd
pkgver=0.1.0
pkgrel=1
pkgdesc="Thermaltake TH420V2 Ultra LCD controller for Linux"
arch=('x86_64' 'aarch64')
url="https://github.com/klatooine/th-linux-lcd"
license=('MIT')
depends=('libusb' 'python' 'python-pillow' 'gcc-libs' 'glibc' 'libglvnd' 'libxkbcommon' 'hicolor-icon-theme')
makedepends=('cargo' 'git')
optdepends=('zenity: file picker for importing GIFs and images'
            'snixembed: tray icon on desktops without StatusNotifierItem support')
install=th-linux-lcd.install
source=("$pkgname::git+$url.git#tag=v$pkgver")
sha256sums=('SKIP')

prepare() {
    cd "$pkgname"
    export RUSTUP_TOOLCHAIN=stable
    cargo fetch --locked --target "$(rustc -vV | sed -n 's/host: //p')"
}

build() {
    cd "$pkgname"
    export RUSTUP_TOOLCHAIN=stable
    export CARGO_TARGET_DIR=target
    cargo build --frozen --release --bin "$pkgname"
}

check() {
    cd "$pkgname"
    export RUSTUP_TOOLCHAIN=stable
    cargo test --frozen --lib
}

package() {
    cd "$pkgname"
    install -Dm755 "target/release/$pkgname" "$pkgdir/usr/bin/$pkgname"
    install -Dm644 pkg/70-thermaltake-lcd.rules "$pkgdir/usr/lib/udev/rules.d/70-thermaltake-lcd.rules"
    install -Dm644 th-linux-lcd.service "$pkgdir/usr/lib/systemd/user/th-linux-lcd.service"
    install -Dm644 pkg/th-linux-lcd.desktop "$pkgdir/usr/share/applications/th-linux-lcd.desktop"
    install -Dm644 pkg/th-linux-lcd-tray.desktop "$pkgdir/etc/xdg/autostart/th-linux-lcd-tray.desktop"
    install -Dm644 pkg/th-linux-lcd.svg "$pkgdir/usr/share/icons/hicolor/scalable/apps/th-linux-lcd.svg"
    install -Dm644 LICENSE "$pkgdir/usr/share/licenses/$pkgname/LICENSE"
}
