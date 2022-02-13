#!/usr/bin/env bash
# Rusolver releaser
NAME="rusnapshot"

LINUX_TARGET="x86_64-unknown-linux-musl"
MANPAGE_DIR="./$NAME.1"

# Linux build
echo "Building Linux artifact."
if cargo build -q --release --target="$LINUX_TARGET"; then
  echo "Linux artifact build: SUCCESS"
  cp "target/$LINUX_TARGET/release/$NAME" "target/$LINUX_TARGET/release/$NAME-linux"
  strip "target/$LINUX_TARGET/release/$NAME-linux"
  sha512sum "target/$LINUX_TARGET/release/$NAME-linux" >"target/$LINUX_TARGET/release/$NAME-linux.sha512"
else
  echo "Linux artifact build: FAILED"
fi

echo "Creating manpage..."
if command -v help2man >/dev/null; then
  if help2man -o "$MANPAGE_DIR" "target/$LINUX_TARGET/release/$NAME"; then
    echo "Manpage created sucessfully and saved in $MANPAGE_DIR"
  else
    echo "Error creating manpage."
  fi
else
  echo "Please install the help2man package."
fi

if command -v git >/dev/null; then
  git add .
  git commit -m "Bump version."
  git push
fi

#echo "Uploading crate to crates.io..."
#if cargo publish --no-verify > /dev/null; then
#  echo "Crate uploaded."
#else
#  echo "An error has occurred while uploading the crate to crates.io."
#  exit
#fi

echo "All builds have passed!"
