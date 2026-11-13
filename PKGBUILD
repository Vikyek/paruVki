# Maintainer: Vikyek
# Based on paru by Morganamilo <morganamilo@archlinux.org>

pkgname=paruvki
pkgver=2.2.0
pkgrel=1
pkgdesc='paruVki — Feature packed AUR helper (fork of paru)'
url='https://github.com/Vikyek/paruVki'
arch=('x86_64' 'aarch64')
license=('GPL-3.0-or-later')
depends=('git' 'pacman>=7.1' 'libalpm.so=16-64')
makedepends=('cargo')
optdepends=(
  'bat: colored pkgbuild printing'
  'devtools: build in chroot and downloading pkgbuilds'
)
provides=('paru')
conflicts=('paru')
backup=('etc/paru.conf')
source=()

build() {
  cd "$startdir"
  cargo build --release --locked
  ./scripts/mkmo locale/
}

package() {
  cd "$startdir"

  install -Dm755 target/release/paruvki "$pkgdir/usr/bin/paruvki"
  ln -s paruvki "$pkgdir/usr/bin/paru"

  install -Dm644 paru.conf "$pkgdir/etc/paru.conf"

  install -Dm644 man/paru.8 "$pkgdir/usr/share/man/man8/paru.8"
  install -Dm644 man/paru.conf.5 "$pkgdir/usr/share/man/man5/paru.conf.5"

  install -Dm644 completions/bash "$pkgdir/usr/share/bash-completion/completions/paru.bash"
  install -Dm644 completions/fish "$pkgdir/usr/share/fish/vendor_completions.d/paru.fish"
  install -Dm644 completions/zsh "$pkgdir/usr/share/zsh/site-functions/_paru"

  install -dm755 "$pkgdir/usr/share/locale"
  cp -r locale/* "$pkgdir/usr/share/locale/"

  install -Dm644 LICENSE "$pkgdir/usr/share/licenses/$pkgname/LICENSE"
}
