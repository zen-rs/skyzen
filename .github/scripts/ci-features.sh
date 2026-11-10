#!/usr/bin/env bash
# The cargo feature selection CI builds, lints and tests the workspace with on this host.
#
# Everywhere but Linux that is every feature of every member. On Linux, `tls-native-tls` is OpenSSL,
# which cannot share a binary with `http3` (quiche's BoringSSL exports the same `SSL_*` symbols), so
# the selection is every feature but the ones that bring OpenSSL in.
set -euo pipefail

case "$(uname -s)" in
  Linux)
    cargo metadata --no-deps --format-version 1 | jq -r '
      ["skyzen/tls-native-tls", "skyzen-services/tls-native-tls"] as $openssl
      | [.packages[] as $package | $package.features | keys[] | "\($package.name)/\(.)"]
      | map(select(. as $feature | $openssl | index($feature) | not))
      | "--features=" + join(",")'
    ;;
  *)
    echo "--all-features"
    ;;
esac
