# WakuWaku on Linux

## Install

```sh
curl -fsSL https://wakuwaku.bingzi.dev/install.sh | sh
```

The script needs no root. It unpacks the release tarball into
`~/.local/wakuwaku.app` and installs the desktop entry into
`~/.local/share/applications`, so **WakuWaku appears in your applications menu** —
you can also launch it from a terminal via `wakuwaku` command. Run the script again to
upgrade; it replaces the previous install rather than merging into it.

WakuWaku expects:

- **glibc 2.35 or newer** — Ubuntu 22.04, Debian 12, Fedora 36, and anything
  more recent. Releases are built on Ubuntu 22.04, so older distributions must
  build from source.
- **A working Vulkan or OpenGL driver.** WakuWaku renders through wgpu, which tries
  Vulkan first and falls back to GL. Software rasterizers (lavapipe, llvmpipe)
  are accepted, so it can run in a VM, but see the note below.
- **x86_64 or aarch64.** Other architectures build from source.
- `xdg-desktop-portal` for native file dialogs.

Set `WAKUWAKU_VERSION` to install a specific version rather than the latest.

## Installing manually

The script is a convenience, not a requirement. Download
`wakuwaku-<version>-<target>.tar.gz` from
[releases.bingzi.dev](https://releases.bingzi.dev) or the
[GitHub release](https://github.com/Cyenoch/WakuWaku/releases), then unpack it
wherever you like:

```sh
mkdir -p ~/.local/wakuwaku.app
tar -xzf wakuwaku-<version>-<target>.tar.gz --strip-components=1 -C ~/.local/wakuwaku.app
ln -sf ~/.local/wakuwaku.app/bin/wakuwaku ~/.local/bin/wakuwaku   # optional
```

The archive uses an install-prefix layout (`bin/`, `share/`) beneath one
versioned directory, so `--strip-components=1` into a prefix such as
`/usr/local` works too.

**Keep `bin/` intact.** WakuWaku launches `wakuwaku-daemon` from its own directory, so
copying `bin/wakuwaku` somewhere on its own leaves it unable to start the daemon.
A symlink is fine — WakuWaku resolves it back to the real path.

Installing the desktop entry is the part that matters — it is how the app is
launched normally, and it is what associates the running window with its icon
and name (WakuWaku reports the Wayland `app_id` / X11 `WM_CLASS` `dev.bingzi.wakuwaku`, which
matches the entry's filename). Install the packaged file and point it at the
install (the packaged copy uses bare `Exec=wakuwaku` and `Icon=dev.bingzi.wakuwaku` names so it
can be relocated):

```sh
install -D ~/.local/wakuwaku.app/share/applications/dev.bingzi.wakuwaku.desktop \
  -t ~/.local/share/applications
sed -i "s|^Exec=wakuwaku$|Exec=$HOME/.local/wakuwaku.app/bin/wakuwaku|" \
  ~/.local/share/applications/dev.bingzi.wakuwaku.desktop
sed -i "s|^Icon=dev.bingzi.wakuwaku$|Icon=$HOME/.local/wakuwaku.app/share/icons/hicolor/256x256/apps/dev.bingzi.wakuwaku.png|" \
  ~/.local/share/applications/dev.bingzi.wakuwaku.desktop
```

## Updating

WakuWaku does not update itself on Linux — Sparkle is macOS-only. Re-run the
install script to upgrade.

## Uninstalling

```sh
curl -fsSL https://wakuwaku.bingzi.dev/install.sh | sh -s -- --uninstall
```

This removes `~/.local/wakuwaku.app`, the symlink, and the desktop entry. Projects
and settings stay in `~/.wakuwaku`; delete that directory to remove them too.

## Building from source

See [CONTRIBUTING.md](../CONTRIBUTING.md) for build prerequisites, then
produce the same archive this page installs with:

```sh
./scripts/bundle-linux.sh
```

To exercise the install script against that local build:

```sh
WAKUWAKU_BUNDLE_PATH=target/release/wakuwaku-<version>-<target>.tar.gz \
  sh website/public/install.sh
```

## Running in a virtual machine

VMs usually have no GPU passthrough, so Mesa falls back to a software
rasterizer. That works in principle — wgpu accepts a CPU adapter — but both
lavapipe (Vulkan) and llvmpipe (GL) JIT-compile shaders through LLVM, and that
path is fragile: on Fedora 44 aarch64 (mesa 26.0.3 + LLVM 22.1) it segfaults
inside `gallivm_jit_function` while compiling a fragment shader. The crash is
in the driver, not in WakuWaku, and no application-side setting avoids it.

If the app dies on its first frame in a VM, check `coredumpctl info` for a
backtrace through `libvulkan_lvp.so` or `libgallium`. The reliable fix is to
give the guest a real GL driver — on UTM that means the QEMU backend with
virtio-gpu-gl (virgl) rather than Apple Virtualization, which offers Linux
guests no 3D at all. `VK_DRIVER_FILES=/nonexistent.json` hides the software
Vulkan driver so wgpu takes the GL path instead.
