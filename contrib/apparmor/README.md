# AppArmor profile for agentbox

`agentbox-nspawn` is an optional AppArmor profile that adds a mandatory-access
-control layer to a box, as **defense in depth**. A box is already contained by a
user namespace, an empty capability set, seccomp and the mount plan; none of
those is an LSM, so this profile is a second wall that stays up even if one of
the others regresses. It is not required, and agentbox runs fine without it.

## What it protects

The profile confines the `systemd-nspawn` process (and, by inheritance, the
box's own PID 1 and everything the box runs). File and mount access is left
broad on purpose - a dev box runs arbitrary toolchains and nspawn needs to
mount and `pivot_root`. The value is a targeted deny-list of host-catastrophic
operations no box workflow needs:

- loading/unloading kernel modules, raw I/O, setting the clock, reboot/kexec,
  and loading or bypassing LSM policy (`sys_module`, `sys_rawio`, `sys_time`,
  `sys_boot`, `mac_admin`, `mac_override`);
- writes to `/boot`, `/sys/firmware`, securityfs, magic SysRq, and the sensitive
  `/proc/sys/kernel/*` knobs;
- reads of `/proc/kcore` and `/dev/{mem,kmem,port}`;
- direct writes to raw disks (`/dev/sd*`, `/dev/nvme*`, `/dev/vd*`, device-mapper).

After nspawn pivots into the box rootfs those host paths are not visible inside
the box, so denying them does not affect normal use - they matter only if the
primary containment ever slips and the box can see the host's real objects.

## This is per-distro

The profile targets **AppArmor**, which Arch (agentbox's host target) and Debian
/Ubuntu/SUSE ship. On a SELinux distro (Fedora/RHEL) none of this applies; leave
`apparmor = false` or simply do not load the profile. AppArmor userspace tooling
(`apparmor_parser`, `aa-complain`, `aa-enforce`, `aa-logprof`) comes from the
`apparmor` package.

## Install and load

The profile has to be loaded into the kernel before systemd can apply it;
agentbox does not load it for you.

**Test in complain mode first.** The author could not test this profile against
a running box, so treat it as a starting point: run a box under it in complain
mode, exercise real work, and review the audit log before enforcing.

```console
# 1. install the profile
$ sudo install -Dm644 contrib/apparmor/agentbox-nspawn /etc/apparmor.d/agentbox-nspawn

# 2. load it in COMPLAIN mode (logs would-be denials, blocks nothing)
$ sudo apparmor_parser -r -C /etc/apparmor.d/agentbox-nspawn
$ sudo aa-complain /etc/apparmor.d/agentbox-nspawn        # equivalent, if you prefer

# 3. exercise a box, then look for anything it wanted to do that the profile denies
$ agentbox shell          # build something, sudo pacman -Syu, git/jj, run your agent
$ sudo aa-logprof                                          # walk the audit log
$ sudo dmesg | grep -i apparmor                            # or read it raw

# 4. once it is clean, switch to ENFORCE
$ sudo aa-enforce /etc/apparmor.d/agentbox-nspawn
```

Confirm it is loaded (this is exactly the check agentbox makes):

```console
$ sudo grep agentbox-nspawn /sys/kernel/security/apparmor/profiles
agentbox-nspawn (enforce)
```

To keep it across reboots, either enable the `apparmor.service` (which loads
everything in `/etc/apparmor.d/`) or add your own loader; that part is
distro-specific.

## How agentbox applies it

Once the profile is loaded, agentbox applies it automatically (the `apparmor`
config key defaults to on-if-available):

- **`agentbox up`** (booted box): a drop-in at
  `/etc/systemd/system/systemd-nspawn@<box>.service.d/60-agentbox-apparmor.conf`
  sets `AppArmorProfile=-agentbox-nspawn`.
- **`agentbox shell` / `run`** (direct launch): a
  `--property=AppArmorProfile=-agentbox-nspawn` on the transient
  `systemd-run --scope` the box runs inside.

The leading `-` makes application non-fatal: if the profile is not loaded, the
box still launches. agentbox only wires the profile in when it is actually
loaded, so hosts that never install it see no change at all.

To opt out on a host that does have it loaded, set `apparmor = false` in
`~/.config/agentbox/config.toml` or a project `.agentbox.toml`. Set
`apparmor = true` to be warned when it is expected but missing.

Check what a given box would get, without changing anything, from a context that
can read the loaded-profile list (i.e. as root):

```console
$ sudo agentbox --dry-run shell
```
