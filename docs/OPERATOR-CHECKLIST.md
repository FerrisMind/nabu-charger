# What has to be done by hand on the tablet

Instructions for a human. The agent works on the development computer and **cannot log
into the Windows session on the tablet by itself**, so there are two paths. The first is
shorter for you: you give the agent access, and from then on it does all the work itself.

---

## Option A (recommended): the agent does everything itself

You need to run **one command** on the tablet and pass the agent the three lines
it prints.

1. Copy the driver package folder to the tablet
   (`G:\nabu-fast-charge\11-driver-rust\artifacts\driver-ln8000-arm64`),
   for example to `C:\nabu-ln8000`.

2. On the tablet open PowerShell **as administrator** and run:

   ```powershell
   cd C:\nabu-ln8000
   .\enable-remote.ps1
   ```

3. The script prints the tablet address, the user name and the password. Pass those
   three lines to the agent - and that is all: it does the installation, the mode check,
   the telemetry collection, the measurements and the analysis itself from this computer.

**What this changes on the tablet:** the WinRM remote management service is enabled,
its port is opened in private networks and a separate local administrator `nabuagent`
is created with a random password. Nothing is reflashed, the partitions and the
bootloader are not touched.

**When we are done - remove the access (on the tablet, as administrator):**

```powershell
Remove-LocalUser -Name nabuagent
Remove-NetFirewallRule -DisplayName 'nabu remote 5985'
Disable-PSRemoting -Force
```

Or ask the agent to run `remote-bringup.ps1 -RemoveAccess` - it will remove the
access itself after collecting the report.

**Security caveat:** WinRM on a local network encrypts traffic weakly.
It is good enough for a trusted home network; on an open network (a cafe, a hotel)
this should not be done - use option B.

---

## Option B: by hand, without remote access

### Step 1. Copy the package

Same thing: the folder `driver-ln8000-arm64` (12 files: the driver, the certificate
and the scripts) is moved to the tablet, for example to `C:\nabu-ln8000`.

### Step 2. Enable test signing

```powershell
cd C:\nabu-ln8000
bcdedit /set testsigning on
Restart-Computer
```

The driver is signed with the WDK test certificate, and without this mode Windows
will not load it.

### Step 3. One run that collects everything

After the reboot, again in PowerShell as administrator:

```powershell
cd C:\nabu-ln8000
.\bring-up.ps1
```

The script does everything itself: it checks the system and the `ACPI\QCOM057E` node, installs
the driver, captures the telemetry, measures the battery charge through WMI twice with an
interval and puts everything into one report with an archive. About three minutes, two of
them are the wait that makes it visible whether the charge is rising or standing still. **For
those two minutes keep the tablet connected to the supply that should provide the fast mode.**

At the end the script prints the paths, for example:

```text
C:\ProgramData\nabu-fastcharge\report\nabu-report-2026-09-16-140000.txt
C:\ProgramData\nabu-fastcharge\report\nabu-report-2026-09-16-140000.zip
```

### Step 4. Send the report (any way)

* **As text** - open the `.txt` in Notepad, copy the contents into the chat.
* **As a file** - put the `.zip` into `G:\nabu-fast-charge\12-reports\` on this
  computer and tell the agent that the file is there.
* **Over the local network** - run `deploy\receive-report.ps1` here
  (as administrator, with the `-OpenFirewall` flag), it prints a ready-to-use command
  for the tablet.

---

## Separate: checking the package without hardware

Any engineer on any Windows computer can run this check - it does not require the
tablet and confirms the integrity of the package:

```powershell
cd G:\nabu-fast-charge\11-driver-rust\deploy
.\verify-package.ps1
```

It checks: that all files are present, the driver bitness (`0xAA64`), the contents of the INF
(the `ACPI\QCOM057E` identifier, the service name, the profile parameters), that the checksums
match and that all scripts parse. Currently: **35 checks, zero problems**.

---

## If something did not work

Send the report anyway - it is valuable exactly when things did not work. It already
contains: the Windows build, the state of the node and the service, the reason the
device failed, the driver telemetry, the charge change over the interval.

| What the report shows | What it means |
|---|---|
| `node not found - the driver has nothing to start on` | there is no `QCOM057E` node in this Windows boot |
| `Problem` is not `0`, the state is not `OK` | the device did not start: look at the problem code in the report |
| `charge is UNCHANGED` | confirmation of the original problem: under Windows the charge does not go |
| `charge is FALLING` | the supply delivers less than the tablet consumes |
| `mode 2:1 did not engage, staying in bypass` | the driver did its job, but the chip did not confirm the fast mode |
| `failed to open \\.\nabu_ln8000 (code 2)` | the driver is not installed or did not start - look at the service in the report |

---

## Rollback

When the results have been captured:

```powershell
cd C:\nabu-ln8000
.\uninstall-driver.ps1
```

The driver will be removed together with the service, and charging returns to the
stock behavior. The removal transcript - `C:\ProgramData\nabu-fastcharge\uninstall.log`.

The test mode can be turned off again:

```powershell
bcdedit /set testsigning off
```

---

## What these steps do not do

* they do not reflash the tablet and do not change the partitions;
* they do not disable driver signature verification forever - only the standard
  Windows test mode, which is turned back off;
* they do not touch user data.
