# Arch packaging

```bash
cd packaging/arch
makepkg -si
```

The history collector is a user service and is not started by installing the
package — enabling something that writes to disk every five seconds should be
the user's decision:

```bash
systemctl --user enable --now clearview-collector
```

Without it the application still works; the Performance tab simply has no
history older than the moment the window opened.

## Publishing to the AUR

The `source=` line points at a release tarball, so tag one first:

```bash
git tag v0.1.0 && git push origin v0.1.0
```

Then replace `sha256sums=('SKIP')` with the real checksum (`makepkg -g`),
generate `.SRCINFO` (`makepkg --printsrcinfo > .SRCINFO`), and push both files
to the AUR repository. `namcap PKGBUILD clearview-*.pkg.tar.zst` will flag
anything the AUR guidelines object to before anyone else has to.
