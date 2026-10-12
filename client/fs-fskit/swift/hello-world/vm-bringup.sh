#!/bin/bash
# VM bring-up for the hello-world FSKit module — DEV-ONLY, for a throwaway VM with
# **AMFI disabled** (`nvram boot-args=amfi_get_out_of_my_way=1`) and **SIP disabled**.
# This is NOT a shipping path: it deliberately bypasses the System-Settings enable toggle
# by writing fskitd's `enabledModules.plist` directly (only legitimate on a disposable VM
# you own — never on a real host). See client/fs-fskit/BUILD.md "VM verification".
#
# What it proves: with AMFI off, an **ad-hoc-signed** FSKit module carrying the restricted
# `com.apple.developer.fskit.fsmodule` entitlement gets PAST the Phase-0 "Module is disabled!"
# wall — it registers, enables, launches, and FSKit accepts the delegate. (The remaining
# blocker is the ExtensionFoundation run-loop / XPC-endpoint vend; see BUILD.md.)
#
# Usage (run on the HOST that built the bundle; needs sshpass):
#   VM_IP=192.168.64.2 ./vm-bringup.sh /tmp/hellofs-build/MakerHelloFS.app
set -euo pipefail

VM_IP="${VM_IP:-192.168.64.2}"
VM_USER="${VM_USER:-admin}"
VM_PASS="${VM_PASS:-admin}"
APP="${1:-/tmp/hellofs-build/MakerHelloFS.app}"
MODULE_ID="${MODULE_ID:-com.maker.fskit.hello.module}"
FS_TYPE="${FS_TYPE:-makerhello}"
MNT="${MNT:-/tmp/hellomnt}"

SSH() { sshpass -p "$VM_PASS" ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null "$VM_USER@$VM_IP" "$@"; }
SCP() { sshpass -p "$VM_PASS" scp -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null "$@"; }

APPNAME="$(basename "$APP")"
echo "==> packaging + copying $APPNAME to the VM"
( cd "$(dirname "$APP")" && tar czf /tmp/_fskit_app.tar.gz "$APPNAME" )
SCP /tmp/_fskit_app.tar.gz "$VM_USER@$VM_IP:/tmp/_fskit_app.tar.gz"

echo "==> installing + registering on the VM"
SSH "echo $VM_PASS | sudo -S umount $MNT 2>/dev/null; echo $VM_PASS | sudo -S rm -rf /Applications/$APPNAME; \
     cd /Applications && echo $VM_PASS | sudo -S tar xzf /tmp/_fskit_app.tar.gz && \
     /System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister -f /Applications/$APPNAME && \
     pluginkit -a /Applications/$APPNAME/Contents/PlugIns/*.appex && sleep 1 && \
     echo '--- pluginkit discovery (expect a + and the module id) ---' && \
     pluginkit -m -p com.apple.fskit.fsmodule | grep -i '${MODULE_ID%%.module}' || true"

echo "==> force-enabling the module by writing fskitd's enabledModules.plist (DEV-ONLY, SIP off)"
SSH "ADMIN=\"\$HOME/Library/Group Containers/group.com.apple.fskit.settings/enabledModules.plist\"; \
     ROOT=\"/var/root/Library/Group Containers/group.com.apple.fskit.settings/enabledModules.plist\"; \
     pluginkit -e use -i $MODULE_ID >/dev/null 2>&1 || true; sleep 1; \
     plutil -p \"\$ADMIN\" | grep -q $MODULE_ID || plutil -insert 5 -string $MODULE_ID \"\$ADMIN\"; \
     echo $VM_PASS | sudo -S sh -c \"plutil -p '\$ROOT' | grep -q $MODULE_ID || plutil -insert 5 -string $MODULE_ID '\$ROOT'\"; \
     echo '--- enabledModules.plist (admin) ---'; plutil -p \"\$ADMIN\""

echo "==> restarting fskitd + mounting"
SSH "echo $VM_PASS | sudo -S killall fskitd 2>/dev/null; sleep 1; mkdir -p $MNT; \
     /sbin/mount -F -t $FS_TYPE none $MNT; echo mount-rc=\$?; \
     mount | grep -i $FS_TYPE && ls -la $MNT && cat $MNT/hello.txt 2>/dev/null || true"

echo "==> done. To inspect failures: log show / log stream filtered on process == \"fskitd\"/\"FskitHello\"."
echo "    To restore baseline: ./vm-restore.sh"
