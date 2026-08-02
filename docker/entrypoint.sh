#!/bin/sh
# Apply a tc netem impairment profile to the container's interface when
# $NETEM is set (needs cap_add: NET_ADMIN), then exec the real command.
# Examples: NETEM="delay 30ms loss 1%"
#           NETEM="delay 80ms 20ms distribution normal loss 3%"
set -e
if [ -n "$NETEM" ]; then
	echo "netem: $NETEM"
	tc qdisc add dev eth0 root netem $NETEM
fi
exec "$@"
