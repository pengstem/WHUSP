#!/usr/bin/env bash
set -euo pipefail

action="${1:-status}"
iface="${STARFIVE_HOST_IFACE:-enp5s0}"
host_cidr="${STARFIVE_HOST_CIDR:-192.168.120.1/24}"
serial_device="${STARFIVE_SERIAL_DEVICE:-/dev/ttyUSB0}"
tftp_root="${STARFIVE_TFTP_ROOT:-/tmp/whusp-starfive-tftp}"
unit="whusp-starfive-tftp.service"
desktop_user="${SUDO_USER:-${STARFIVE_DESKTOP_USER:-nastem}}"
desktop_group="$(id -gn "$desktop_user")"

require_root() {
    if [ "$(id -u)" -ne 0 ]; then
        echo "run this action through sudo" >&2
        exit 1
    fi
}

case "$action" in
    up)
        require_root
        ip link show dev "$iface" >/dev/null
        install -d -o "$desktop_user" -g "$desktop_group" "$tftp_root"
        ip address replace "$host_cidr" dev "$iface"
        if [ -e "$serial_device" ]; then
            setfacl -m "u:$desktop_user:rw" "$serial_device"
        fi
        systemctl stop "$unit" 2>/dev/null || true
        systemctl reset-failed "$unit" 2>/dev/null || true
        systemd-run \
            --unit="${unit%.service}" \
            --property=Restart=on-failure \
            /usr/bin/dnsmasq \
            --keep-in-foreground \
            --port=0 \
            --enable-tftp \
            --tftp-root="$tftp_root" \
            --interface="$iface" \
            --bind-interfaces \
            --no-hosts \
            --no-resolv \
            --user="$desktop_user" \
            --group="$desktop_group"
        ;;
    down)
        require_root
        systemctl stop "$unit" 2>/dev/null || true
        ip address del "$host_cidr" dev "$iface" 2>/dev/null || true
        if [ -e "$serial_device" ]; then
            setfacl -x "u:$desktop_user" "$serial_device" 2>/dev/null || true
        fi
        ;;
    status)
        ip -brief address show dev "$iface"
        systemctl is-active "$unit" || true
        if [ -e "$serial_device" ]; then
            getfacl -p "$serial_device"
        else
            echo "serial device absent: $serial_device"
        fi
        ;;
    *)
        echo "usage: $0 {up|down|status}" >&2
        exit 2
        ;;
esac
