# Maintainer: Ricarsz <Ricarsz@users.noreply.github.com>
pkgname=sesstui
pkgver=0.5.0
pkgrel=1
pkgdesc='Terminal UI to browse, filter and delete DeepSeek Harness, Codex and Opencode sessions'
arch=('x86_64' 'aarch64')
url='https://github.com/Ricarsz/sesstui'
license=('MIT')
depends=('gcc-libs' 'glibc')
makedepends=('cargo')
optdepends=('zstd: fallback decoder for DeepSeek Harness session logs')
options=(!lto)
source=("$pkgname::git+$url#tag=v$pkgver")
sha256sums=('SKIP')

prepare() {
  cd "$pkgname"
  cargo fetch --locked
}

build() {
  cd "$pkgname"
  export RUSTFLAGS="${RUSTFLAGS:-} --remap-path-prefix=$srcdir=/build/src --remap-path-prefix=$HOME=/build/home"
  cargo build --release --locked --frozen
}

check() {
  cd "$pkgname"
  cargo test --release --locked --frozen
}

package() {
  cd "$pkgname"
  install -Dm755 target/release/sesstui "$pkgdir/usr/bin/sesstui"
  install -Dm644 LICENSE "$pkgdir/usr/share/licenses/$pkgname/LICENSE"
  install -Dm644 README.md "$pkgdir/usr/share/doc/$pkgname/README.md"
}
