#!/bin/sh
# certbot deploy hook for a no_master server: copy the issued or renewed
# certificate into the server's ./data/tls, where it serves it and picks up
# each renewal within the hour without a restart. Installed on the host as
# /usr/local/bin/digblock-cert-hook and named by --deploy-hook at issue, so
# certbot keeps running it on every renewal. DIGBLOCK_TLS_DIR overrides the
# destination for a server living in another folder.
set -eu
dst="${DIGBLOCK_TLS_DIR:-/home/debian/digblock/data/tls}"
install -d -m 700 "$dst"
# Written beside, then renamed: the server never reads half a file.
install -m 644 "$RENEWED_LINEAGE/fullchain.pem" "$dst/fullchain.pem.new"
install -m 600 "$RENEWED_LINEAGE/privkey.pem" "$dst/privkey.pem.new"
mv "$dst/fullchain.pem.new" "$dst/fullchain.pem"
mv "$dst/privkey.pem.new" "$dst/privkey.pem"
