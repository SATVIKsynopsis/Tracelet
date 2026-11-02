set -eu

REPO="SATVIKsynopsis/Tracelet"
BIN="otel-test"
VERSION="${OTEL_TEST_VERSION:-latest}"
INSTALL_DIR="${OTEL_TEST_INSTALL_DIR:-$HOME/.local/bin}"

err() {
  echo "error: $*" >&2
  exit 1
}

# Detect platform
os=$(uname -s)
arch=$(uname -m)

case "$os" in
  Linux) os="unknown-linux-gnu" ;;
  Darwin) os="apple-darwin" ;;
  *) err "unsupported OS: $os. On Windows, download the .zip from https://github.com/$REPO/releases" ;;
esac

case "$arch" in
  x86_64 | amd64) arch="x86_64" ;;
  arm64 | aarch64) arch="aarch64" ;;
  *) err "unsupported architecture: $arch" ;;
esac

target="$arch-$os"
name="$BIN-$target"

if [ "$VERSION" = "latest" ]; then
  base="https://github.com/$REPO/releases/latest/download"
else
  base="https://github.com/$REPO/releases/download/$VERSION"
fi

# Downloader
if command -v curl >/dev/null 2>&1; then
  fetch() { curl -fsSL "$1" -o "$2"; }
elif command -v wget >/dev/null 2>&1; then
  fetch() { wget -qO "$2" "$1"; }
else
  err "curl or wget is required"
fi

# Checksum tool
if command -v sha256sum >/dev/null 2>&1; then
  sha256() { sha256sum "$1" | cut -d' ' -f1; }
elif command -v shasum >/dev/null 2>&1; then
  sha256() { shasum -a 256 "$1" | cut -d' ' -f1; }
else
  err "sha256sum or shasum is required"
fi

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

echo "Downloading $name ($VERSION)..."
fetch "$base/$name.tar.gz" "$tmp/$name.tar.gz" || err "download failed: $base/$name.tar.gz"
fetch "$base/$name.tar.gz.sha256" "$tmp/$name.tar.gz.sha256" || err "checksum download failed"

expected=$(cut -d' ' -f1 "$tmp/$name.tar.gz.sha256")
actual=$(sha256 "$tmp/$name.tar.gz")
[ "$expected" = "$actual" ] || err "checksum mismatch (expected $expected, got $actual)"

tar xzf "$tmp/$name.tar.gz" -C "$tmp"
mkdir -p "$INSTALL_DIR"
cp "$tmp/$name/$BIN" "$INSTALL_DIR/$BIN"
chmod 755 "$INSTALL_DIR/$BIN"

echo "Installed $BIN to $INSTALL_DIR/$BIN"

case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *)
    echo ""
    echo "$INSTALL_DIR is not on your PATH. Add this to your shell profile:"
    echo "  export PATH=\"$INSTALL_DIR:\$PATH\""
    ;;
esac

echo ""
echo "Get started:  $BIN init && $BIN test"