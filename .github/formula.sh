#!/usr/bin/env bash
# formula.sh VERSION DIST: print the Homebrew formula for VERSION, with the
# SHA-256 of each archive read from DIST/sluice-VERSION-<target>.tar.gz.sha256.
set -euo pipefail
version=$1 dist=$2
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "bad version: $version" >&2; exit 1; }

sum() {
  local f="$dist/sluice-$version-$1.tar.gz.sha256" s
  s=$(cut -d' ' -f1 "$f")
  [[ "$s" =~ ^[0-9a-f]{64}$ ]] || { echo "bad sum in $f" >&2; exit 1; }
  echo "$s"
}
url="https://github.com/rashedInt32/sluice/releases/download/v$version/sluice-$version"

cat <<EOF
# Written by sluice's release workflow; edits here are overwritten.
class Sluice < Formula
  desc "Tmux-style sessions and prefix keys on top of zmx, with raw PTY passthrough"
  homepage "https://github.com/rashedInt32/sluice"
  license "MIT"

  depends_on "fzf"
  depends_on "neurosnap/tap/zmx"

  on_macos do
    on_arm do
      url "$url-aarch64-apple-darwin.tar.gz"
      sha256 "$(sum aarch64-apple-darwin)"
    end
    on_intel do
      url "$url-x86_64-apple-darwin.tar.gz"
      sha256 "$(sum x86_64-apple-darwin)"
    end
  end

  on_linux do
    on_arm do
      url "$url-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "$(sum aarch64-unknown-linux-gnu)"
    end
    on_intel do
      url "$url-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "$(sum x86_64-unknown-linux-gnu)"
    end
  end

  def install
    bin.install "bin/sluice"
    pkgshare.install "share/sluice/bin", "share/sluice/config.example"
  end

  def caveats
    <<~TEXT
      Type \`sluice\` to start. With no sessions it opens the project picker.

      Config lives in ~/.config/sluice/config; copy only the lines you change
      from the example, which lists every default:
        #{opt_pkgshare}/config.example

      prefix+u, the Claude agent picker, needs jq: brew install jq
    TEXT
  end

  test do
    assert_match "sluice #{version}", shell_output("#{bin}/sluice --version")
  end
end
EOF
