#!/bin/bash
# Restore the throwaway FSKit-test VM to baseline after vm-bringup.sh: unmount, remove the
# test app, and strip our module from fskitd's enabledModules.plist (both admin + root).
set -euo pipefail
VM_IP="${VM_IP:-192.168.64.2}"; VM_USER="${VM_USER:-admin}"; VM_PASS="${VM_PASS:-admin}"
MODULE_ID="${MODULE_ID:-com.maker.fskit.hello.module}"
APPNAME="${APPNAME:-MakerHelloFS.app}"; MNT="${MNT:-/tmp/hellomnt}"
SSH() { sshpass -p "$VM_PASS" ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null "$VM_USER@$VM_IP" "$@"; }

SSH "echo $VM_PASS | sudo -S umount $MNT 2>/dev/null || true; \
     echo $VM_PASS | sudo -S rm -rf /Applications/$APPNAME; rmdir $MNT 2>/dev/null || true; \
     ADMIN=\"\$HOME/Library/Group Containers/group.com.apple.fskit.settings/enabledModules.plist\"; \
     ROOT=\"/var/root/Library/Group Containers/group.com.apple.fskit.settings/enabledModules.plist\"; \
     for P in \"\$ADMIN\"; do while plutil -p \"\$P\" | grep -q $MODULE_ID; do \
        N=\$(plutil -p \"\$P\" | grep -n $MODULE_ID | head -1 | sed -E 's/^ *([0-9]+).*/\1/'); \
        plutil -remove \$((N-1)) \"\$P\" 2>/dev/null || break; done; done; \
     echo $VM_PASS | sudo -S sh -c \"while plutil -p '\$ROOT' | grep -q $MODULE_ID; do N=\\\$(plutil -p '\$ROOT' | grep -n $MODULE_ID | head -1 | sed -E 's/^ *([0-9]+).*/\\\\1/'); plutil -remove \\\$((N-1)) '\$ROOT' 2>/dev/null || break; done\"; \
     echo '--- baseline enabledModules (admin) ---'; plutil -p \"\$ADMIN\""
echo "restored."
