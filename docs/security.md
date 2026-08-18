# Security model

## Threat model

v1 targets untrusted Linux guest code invoked by a local user on that same user's macOS host. The host OS, hypervisor, signed helper, libkrun, libkrunfw, fastmvm supervisor, image preparation code, and invoking user account are trusted.

The following are out of scope for v1:

- hostile host users or a remote multi-tenant control plane;
- protection from a hypervisor or VMM escape;
- macOS or Windows guests;
- a live writable host mount;
- GPU and arbitrary host-device forwarding.

## Host filesystem

libkrun documents that the guest and VMM share a host security context and that virtio-fs alone does not stop a guest from attempting access outside the exported directory on the same host filesystem. Therefore fastmvm does not pass an original host workspace path to libkrun.

The required workspace pipeline is:

1. Open and walk the host tree without following symlinks.
2. Reject path traversal, devices, sockets, and FIFOs.
3. Materialize a dedicated filesystem image and calculate its digest.
4. Reopen it read-only and configure a read-only virtio-block device.
5. Mount it read-only in the guest at `/workspace`.
6. Recheck the host digest in end-to-end tests.

## Network

No network interface is added by default. Because libkrun can automatically enable Transparent Socket Impersonation when no NIC exists, the helper must explicitly create the control vsock with TSI flags set to zero. The release gate exercises IPv4, IPv6, DNS, and AF_UNIX rather than trusting configuration alone.

## Process and resource controls

- VM wall deadline: one hour by default; unlimited only by explicit request.
- Stop escalation: TERM, five-second grace, then KILL.
- RAM/vCPU limits at VM creation; PID and process policy in the guest agent.
- Fixed scratch capacity and bounded output spool.
- Empty guest environment by default; explicit values are never written to trace output.
- Image digests and layer content are verified before use.
- The helper polls its supervisor PID and exits with the VM after owner loss.

Writable workspace overlays and copy-out are not implemented in this vertical slice; the secure behavior is to reject guest writes rather than mutate or directly mount the host source.

## Native dependency gate

`fastmvm doctor` checks the host platform, Hypervisor.framework, helper entitlement, libkrun/libkrunfw locations, and required ABI symbols. Absence keeps the deterministic process backend usable but native execution unavailable. The process backend is never reported as isolated.
