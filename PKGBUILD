# Maintainer: klatooine
pkgname=th-linux-lcd
pkgver=0.1.0
pkgrel=1
pkgdesc="Thermaltake TH420V2 Ultra LCD controller for Linux"
arch=('x86_64')
url="https://github.com/klatooine/th-linux-lcd"
license=('MIT')
depends=('libusb' 'python-pillow')
makedepends=('rust' 'cargo')
optdepends=('zenity: file picker dialogs for importing GIFs and images')
install=th-linux-lcd.install
source=("$pkgname::git+$url.git")
sha256sums=('SKIP')

build() {
    cd "$pkgname"
    cargo build --release --locked
}

package() {
    cd "$pkgname"

    # Binary
    install -Dm755 "target/release/$pkgname" "$pkgdir/usr/bin/$pkgname"

    # udev rule (non-root USB access)
    install -Dm644 "pkg/99-thermaltake-lcd.rules" \
        "$pkgdir/usr/lib/udev/rules.d/99-thermaltake-lcd.rules"

    # Systemd user service
    install -Dm644 "th-linux-lcd.service" \
        "$pkgdir/usr/lib/systemd/user/th-linux-lcd.service"

    # Desktop entry
    install -Dm644 "pkg/th-linux-lcd.desktop" \
        "$pkgdir/usr/share/applications/th-linux-lcd.desktop"

    # License
    install -Dm644 "LICENSE" "$pkgdir/usr/share/licenses/$pkgname/LICENSE" 2>/dev/null || true
}
