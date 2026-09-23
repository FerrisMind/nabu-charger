# Rollback snapshot

`oem154-20.47.10.660/` was the driver package installed on the development tablet when it
was captured, copied out of the DriverStore (`pnputil /enum-drivers` listed it as
`oem154.inf`) so that it can be put back if a later build misbehaves. The tablet has moved
on since: it now runs `oem166.inf` / `20.47.10.672`, the in-tree 0.3.1 build, after
`oem159` / `20.47.10.665` was the first package there with the device access policy in
force.

Only the INF is committed. `*.sys` and `*.cat` are build outputs and `.gitignore`
excludes them, so this directory cannot be reinstalled from a clone — the binaries stay
on the machine that captured them. The INF is here to be read: compare its `[Version]`,
its device list and its hardware key against `crates/ln8000-kmdf/ln8000_kmdf.inx`.

Its `DriverVer` is `09/19/2026,20.47.10.660`, several releases behind the current source
(`09/22/2026,20.47.10.671`). That makes it a snapshot of what was installed at the time
rather than a copy of the current build, and it means the INF cannot be installed over a
newer package without forcing the version: the loader refuses the lower `DriverVer` as
`Outranked`.

## It predates the device access policy

This snapshot carries no `Security` value in its hardware key, and the
`ln8000_kmdf.sys` beside it was built before the driver set a descriptor on the device
object (`crates/ln8000-kmdf/src/sddl.rs`). Both layers of the policy are missing from
it, so rolling back to this package reopens the pump to every process on the tablet:
the control codes are `FILE_ANY_ACCESS` and the driver makes no requestor check.

Treat a rollback as a step on the way to a working build, not as a state to sit in. If
the rollback has to stay, the replacement is a rebuilt package with the policy in it —
see [../docs/DEPLOY-LN8000.md](../docs/DEPLOY-LN8000.md).
