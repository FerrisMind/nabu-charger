# Rollback snapshot

`oem154-20.47.10.660/` is the driver package that is installed on the development
tablet, copied out of the DriverStore (`pnputil /enum-drivers` lists it as `oem154.inf`)
so that it can be put back if a later build misbehaves.

Only the INF is committed. `*.sys` and `*.cat` are build outputs and `.gitignore`
excludes them, so this directory cannot be reinstalled from a clone — the binaries stay
on the machine that captured them. The INF is here to be read: compare its `[Version]`,
its device list and its hardware key against `crates/ln8000-kmdf/ln8000_kmdf.inx`.

Its `DriverVer` is `09/19/2026,20.47.10.660`, the same as the current source. That makes
it a snapshot of what is installed rather than an older release, and it means the INF
cannot be reinstalled over the current one without forcing the version.

## It predates the device access policy

This snapshot carries no `Security` value in its hardware key, and the
`ln8000_kmdf.sys` beside it was built before the driver set a descriptor on the device
object (`crates/ln8000-kmdf/src/sddl.rs`). Both layers of the policy are missing from
it, so rolling back to this package reopens the pump to every process on the tablet:
the control codes are `FILE_ANY_ACCESS` and the driver makes no requestor check.

Treat a rollback as a step on the way to a working build, not as a state to sit in. If
the rollback has to stay, the replacement is a rebuilt package with the policy in it —
see [../docs/DEPLOY-LN8000.md](../docs/DEPLOY-LN8000.md).
