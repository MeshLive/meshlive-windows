## What and why

<!-- The problem, with evidence that it is real (a failing test, a log excerpt with identifiers removed, a measurement). -->

## How it was tested

<!-- Commands run, and for Bluetooth, media or lifecycle changes: the PC, adapter, phones and the same-settings comparison. -->

## Checklist

- [ ] The change is narrow and a focused regression covers it.
- [ ] `scripts\Build.ps1 -Action Test -Configuration Release` passes.
- [ ] New behaviour is default off, labelled Experimental and registered in `docs/dev/switches.md`.
- [ ] Protocol or security changes link to a reviewed design in meshlive-android.
- [ ] The documents that own the changed behaviour are updated.
- [ ] Commits are signed off (`git commit -s`), the diff contains no private data (addresses, serials, IDs, paths,
      names), and substantial AI assistance is disclosed.
