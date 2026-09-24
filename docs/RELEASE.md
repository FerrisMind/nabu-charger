# Release procedure

This is how an installable ARM64 package is produced and published. It is written for
the maintainer; a user who only wants to install the driver needs
[../README.md](../README.md) and [DEPLOY-LN8000.md](DEPLOY-LN8000.md).

---

## 1. Two version numbers, and why both exist

| Number | Where it lives | What it means |
|---|---|---|
| Project version, e.g. `0.3.0` | `[workspace.package] version` in `Cargo.toml`, inherited by every crate | the repository's release, following [SemVer](https://semver.org/) |
| Windows driver version, e.g. `20.47.10.662` | `DriverVer` in `crates/ln8000-kmdf/ln8000_kmdf.inx` **and** `STAMPINF_VERSION` in `deploy/build-arm64.ps1` | what the Windows driver loader compares against the DriverStore |

They are independent on purpose. The Cargo version describes the source; the Windows
version is a four-part number that has to outrank what the target machine already has.

**`STAMPINF_VERSION` wins.** `cargo-wdk` passes it to `stampinf`, which overwrites the
`DriverVer` in the generated INF, so the value in the `.inx` is a fallback and a record
of what the release shipped. Without `STAMPINF_VERSION`, `cargo-wdk` passes
`stampinf -v *` (wall-clock) and a morning rebuild can land below the installed package.

### The trap this guards against

If the new `DriverVer` is lower than the package already in the DriverStore, the install
does not fail loudly - it is skipped as `Status - Outranked` and the device keeps running
the old driver. Check what the target has before choosing the number:

```powershell
Get-ChildItem C:\Windows\INF\oem*.inf |
  ForEach-Object {
    $t = Get-Content $_.FullName -Raw
    if ($t -match 'ln8000_kmdf') {
      $_.Name + '=' + ([regex]::Match($t, 'DriverVer\s*=\s*([^\r\n]+)')).Groups[1].Value.Trim()
    }
  }
```

The same query is how the deployed version was confirmed for this repository: the tablet
carried `oem125` through `oem159` while the driver was being brought up, `oem159`
(`20.47.10.665`) was the first package installed there with the access policy in force, and
`oem165` (`20.47.10.671`) was the package installed before it, and `oem166`
(`20.47.10.672`) is the package installed and verified on it today.

---

## 2. Bump

1. Decide the SemVer bump from the changelog. At `0.y.z` a breaking change bumps `y`:
   the device access policy is one, which is why the release after `0.2.2` is `0.3.0`.
2. `[workspace.package] version` in `Cargo.toml`. Every crate inherits it, including
   `spb`, which used to pin its own. `crates/kmdf` and `crates/ln8000-kmdf` are separate
   workspaces and carry their own `version`, so set those too.
3. `STAMPINF_VERSION` in `deploy/build-arm64.ps1`, and the `DriverVer` line in
   `crates/ln8000-kmdf/ln8000_kmdf.inx`, **to the same value**. Nothing keeps them in
   step automatically.
4. Move `## [Unreleased]` in [CHANGELOG.md](../CHANGELOG.md) to a dated
   `## [x.y.z] - YYYY-MM-DD` section.

---

## 3. Build

```powershell
cd G:\nabu-fast-charge\11-driver-rust
.\deploy\build-arm64.ps1
```

The script builds both drivers for `aarch64-pc-windows-msvc`, copies the packages into
`artifacts/`, writes `artifacts/SHA256SUMS.txt` and `artifacts/BUILD-MANIFEST.json`, and
then **builds both a second time** and compares the two, recording the verdict in the
manifest's `reproducible` field.

The comparison is real, which took two fixes to get right. It reads the first build's
`.sys` out of the crate's own `target\` tree (the second build overwrites it, and no
copy-back puts it under `artifacts/`), and before the second build it deletes
`target\aarch64-pc-windows-msvc\release` so that the second build actually compiles: a
cached `cargo wdk build` finishes in 0.12 s and only re-signs the file that is already
there, and the block then refuses to continue unless the rebuild prints
`Compiling <crate>`. Both images are walked through `Compare-PeImage`, a helper that
ignores exactly two fields - the Authenticode certificate table, which carries a
timestamp, and the PE checksum, which covers that table - and the full bytes are hashed
as well, so a difference inside those fields is not mistaken for equality. Anything else
differing is a genuine difference and the note says `NOT reproducible`.

Measured on this machine, and the reason the field reads `true` today: every section of
the image (`.text`, `fothk`, `.rdata`, `.data`, `.pdata`, `.edata`, `INIT`, `.reloc`) is
identical between two builds and between the released package and the binary installed on
the tablet. Only the signature and the checksum move. The shipped archive is therefore the
code that was verified on hardware, not merely an equivalent of it.

It needs the WDK, `cargo-wdk` and LLVM **17.0.6** - see the requirements table in
[../README.md](../README.md).

Running the script from a shell that leaves `$PSScriptRoot` empty (Git Bash, for
instance) fails at the first `Join-Path`; pass the root explicitly:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass `
  -File "G:\nabu-fast-charge\11-driver-rust\deploy\build-arm64.ps1" `
  -RepoRoot "G:\nabu-fast-charge\11-driver-rust"
```

---

## 4. Check the package before it goes out

These are the checks that catch the mistakes this project has actually made:

| Check | Why |
|---|---|
| `DriverVer` in the built `ln8000_kmdf.inf` equals the intended value | `STAMPINF_VERSION` is easy to leave behind; the built INF is the truth |
| The built INF contains `HKR,,Security,,"D:P(A;;GA;;;SY)(A;;GA;;;BA)"` | a build made before the access-policy commit carries no `Security` value, and installing it reopens the pump to every process on the tablet |
| Machine field of the PE header is `0xAA64` | a host-architecture build links and signs but will not load |
| `Get-AuthenticodeSignature` reports `Valid` for the `.sys` and the `.cat` | test signing has to be on at the target for this to matter, but a broken signature fails earlier |
| `.\deploy\verify-package.ps1` | the repository's own package check, 35 criteria |

The machine-field check, without opening a disassembler:

```powershell
$sys = "artifacts/driver-ln8000-arm64/ln8000_kmdf.sys"
$b = [IO.File]::ReadAllBytes($sys); $pe = [BitConverter]::ToInt32($b, 0x3C)
"Machine = 0x{0:X4}" -f [BitConverter]::ToUInt16($b, $pe + 4)   # 0xAA64 = ARM64
```

---

## 5. Assemble the archive

`deploy/build-arm64.ps1` leaves two directories that are build outputs and are not
tracked by git:

| Directory | Contents | Installable |
|---|---|---|
| `artifacts/driver-ln8000-arm64/` | LN8000 package (`.sys`, `.inf`, `.cat`, test certificate) plus the install, update, uninstall, bring-up, diagnostics and acceptance scripts | yes |
| `artifacts/driver-arm64/` | the SMB detection driver (`.sys`, `.inf`, `.cat`, certificate) | no - no install procedure, and its IOCTLs are stubs |

The release archive is assembled from them:

```powershell
.\deploy\assemble-release.ps1 -Version 0.3.1
```

Invoked from the repository root it resolves its own `-RepoRoot`. Invoked as
`powershell -File <path>` from elsewhere, `$PSScriptRoot` came out empty and the default
`-RepoRoot` failed to bind, so pass `-RepoRoot <repo>` explicitly there.

It writes `artifacts/release/nabu-ln8000-driver-<version>-arm64.zip` with the LN8000 kit
at the top level, the SMB driver under `smb-detection/` with a note saying it is not
installable, `SHA256SUMS.txt`, the build manifest and a copy of this document's install
summary. The script refuses to run if the built INF has no `Security` value or the
machine field is not `0xAA64`.

---

## 6. Publish

The preferred route is the **release** workflow, run by hand: Actions → release → *Run
workflow*, on `master`, with `version` set to the version bumped in step 2. It does steps 3-5
on a runner and then leaves a **draft** release with the archive attached:

* it refuses to run unless the tree is bumped to that version (the four places of step 2),
  and takes the driver stamp from the `.inx`, so the built INF cannot carry a different
  DriverVer than the source;
* it builds both drivers twice and fails unless the reproducibility verdict is `true`;
* it runs `verify-package.ps1` (35 criteria) and the four checks of step 4 - the built
  `DriverVer`, the INF's access policy, the PE machine field and the signature. The
  signature check is stricter than a local one: the package has to be signed, the signature
  has to be intact, and the certificate it was made with has to be the certificate the
  archive ships, or the install stops with `CERT_E_UNTRUSTEDROOT`;
* `deploy/prepare-test-signing.ps1` runs before the build and only removes cached files.
  `cargo-wdk` test-signs with a certificate from the *user* store `WDRTestCertStore` - the one
  already there, or a new one from `makecert` - and it skips making one when it finds a
  `WDRLocalTestCert.cer` in the output directory, which the build cache restores. A machine
  whose store holds no certificate then fails inside the package step with
  `SignTool Error: File not found`. Creating the certificate in the workflow instead does not
  work: `certmgr` and `signtool` read different stores on a runner, `cargo-wdk` adds a second
  certificate with the same subject, and signtool refuses the package with `Multiple
  certificates were found`. No trust store is touched - a test-signed package is untrusted on
  the machine that builds it, and it becomes trusted on the tablet, from the `.cer` in the
  archive;
* it assembles the archive and attaches it to the run as an artifact first, so a failure in
  the release step still leaves the archive downloadable;
* the release notes are the changelog's own `## [<version>]` section plus what the archive
  is, which driver it carries, the reproducibility note and the fact that known defects are
  open;
* a published release for the same tag is never touched - the job stops. An existing draft
  is updated in place, so a re-run after a failure does not need the release deleted;
* `dry_run: true` does everything except create or edit a release: build, check, pack,
  artifact. Use it to inspect an archive without touching the Releases page.

The tag is created together with the draft. A draft that is then abandoned leaves a
`v<version>` tag behind; delete the tag if that version is not going to be published.

By hand, the same three steps, which is what the workflow automates:

1. Tag the commit `v<version>` and push it with the branch.
2. Create the release on GitHub with that tag, attach the zip from step 5, and paste the
   `## [<version>]` section of the changelog as the release notes.
3. Say in the notes which driver version the archive carries and what the known defects
   are. The AC-verdict flap is open; a release note that omits it would be the only
   dishonest document in this repository.

The repository is public. Making it public was a deliberate step, taken once the archive,
the READMEs and the defect lists described the state honestly.
